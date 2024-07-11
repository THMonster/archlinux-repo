use clap::Parser;
use log::*;
use std::fs;
use std::{collections::HashMap, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Set config file
    #[clap(short = 'd', long = "repo-dir", value_parser, value_name = "String")]
    repo_dir: String,

    /// Set bind address
    #[clap(
        short = 'b',
        long = "bind-address",
        value_parser,
        value_name = "String"
    )]
    bind_address: String,

    /// Set github repo
    #[clap(short = 'g', long = "github-repo", value_parser, value_name = "String")]
    github_repo: String,

    /// Set bind address
    #[clap(long = "log-level", default_value_t = 3, value_parser)]
    log_level: u8,
}

async fn vercmp(vernew: &str, verold: &str) -> anyhow::Result<isize> {
    let output = Command::new("vercmp").arg(vernew).arg(verold).output();
    let output = output.await?;
    let ret = std::str::from_utf8(&output.stdout)?.trim();
    let ret: isize = ret.parse()?;
    Ok(ret)
}

fn decode_pkgname(pkg: &str) -> String {
    let v = pkg
        .rsplitn(4, '-')
        .collect::<Vec<&str>>()
        .last()
        .unwrap()
        .to_string();
    return v;
}

async fn refine_raw_pkglist(raw_list: Vec<String>) -> anyhow::Result<HashMap<String, String>> {
    let mut ret: HashMap<String, String> = HashMap::new();
    for pkg in raw_list.into_iter() {
        let pkg_name = decode_pkgname(&pkg);
        if ret.contains_key(&pkg_name) {
            if vercmp(&pkg, ret.get(&pkg_name).unwrap()).await? > 0 {
                ret.insert(pkg_name, pkg);
            }
        } else {
            ret.insert(pkg_name, pkg);
        }
    }
    Ok(ret)
}

async fn repo_add(base_dir: &str, github_repo: &str, pkg_name: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let mut resp = client
        .get(format!(
            r"https://github.com/{}/releases/download/packages/{}",
            github_repo, pkg_name
        ))
        .header("User-Agent", UA)
        .send()
        .await?;
    {
        let mut f = tokio::fs::File::create(format!("{}/x86_64/{}", &base_dir, &pkg_name)).await?;
        while let Some(chunk) = resp.chunk().await? {
            f.write_all(&chunk).await?;
        }
        f.sync_all().await?;
    }
    let rn = github_repo.split('/').next().unwrap_or("myrepo");
    Command::new("repo-add")
        .arg(format!("{}/x86_64/{}.db.tar.gz", base_dir, rn))
        .arg(format!("{}/x86_64/{}", base_dir, pkg_name))
        .spawn()?
        .wait()
        .await?;
    Ok(())
}

async fn sync_repo(base_dir: &str, github_repo: &str) -> anyhow::Result<()> {
    let paths = fs::read_dir(format!("{}/x86_64", &base_dir))?;
    let mut name_list = Vec::new();
    for path in paths {
        let n: String = path.unwrap().file_name().into_string().unwrap();
        if n.ends_with("pkg.tar.zst") {
            name_list.push(n);
        }
    }
    let local_pkg_dict = refine_raw_pkglist(name_list).await?;

    let mut name_list = Vec::new();
    let client = reqwest::Client::new();
    let resp = client
        .get(format!(
            "https://api.github.com/repos/{}/releases",
            github_repo
        ))
        .header("User-Agent", UA)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    if resp[0]["assets"].is_array() != true {
        return Ok(());
    }
    for a in resp[0]["assets"].as_array().unwrap() {
        name_list.push(a.get("name").unwrap().as_str().unwrap().to_string());
    }
    let github_pkg_dict = refine_raw_pkglist(name_list).await?;

    // println!("{:?} {:?}", &github_pkg_dict, &local_pkg_dict);
    for (k, v) in github_pkg_dict.into_iter() {
        if local_pkg_dict.contains_key(&k) {
            let oldpkg_name = local_pkg_dict.get(&k).unwrap();
            if vercmp(&v, &oldpkg_name).await? > 0 {
                let _ = fs::remove_file(format!("{}/x86_64/{}", &base_dir, &oldpkg_name));
                repo_add(base_dir, github_repo, &v).await?;
            }
        } else {
            repo_add(base_dir, github_repo, &v).await?;
        }
    }

    Ok(())
}

fn main() {
    let args = Args::parse();
    let log_level = match args.log_level {
        1 => LevelFilter::Debug,
        2 => LevelFilter::Info,
        3 => LevelFilter::Warn,
        4 => LevelFilter::Error,
        _ => LevelFilter::Info,
    };
    env_logger::Builder::new().filter(None, log_level).init();
    std::fs::create_dir_all(format!("{}/x86_64", &args.repo_dir)).unwrap();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let t1 = async {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    match sync_repo(&args.repo_dir, &args.github_repo).await {
                        Ok(_) => {}
                        Err(e) => warn!("{}", e),
                    };
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            };
            let t2 = async {
                let serve_dir = tower_http::services::ServeDir::new(&args.repo_dir);
                let app = axum::Router::new()
                    // .route("/", axum::routing::get(|| async { "Hi from /foo" }))
                    .nest_service("/repo", serve_dir);
                // .fallback(|| async { "Hi from /fallback" });

                let listener = tokio::net::TcpListener::bind(args.bind_address)
                    .await
                    .unwrap();
                axum::serve(listener, app).await.unwrap();
            };
            tokio::select! {
                _ = t1 => {},
                _ = t2 => {},
            }
            // axum::Server::bind(&args.bind_address.parse().unwrap())
            //     .serve(app.into_make_service())
            //     .await
            //     .unwrap();
        });
}
