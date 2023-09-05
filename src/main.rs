mod download;
mod upload;
use openssh::{KnownHosts, Session};
use openssh_sftp_client::Error;
use openssh_sftp_client::{Sftp, SftpOptions};
use pathdiff::diff_paths;
use rlimit::Resource;
use ssh2_config::SshConfig;
use ssh2_config::{HostParams, ParseRule};
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tokio::fs::File;
use users::{get_current_uid, get_user_by_uid};

#[derive(Debug)]
pub struct Connection {
    remote_path: PathBuf,
    // always save an absolute path
    local_path: PathBuf,
}

impl Connection {
    fn new(remote_path: Option<&str>, local_path: &str, remote_home: Option<String>) -> Self {
        let mut local_path_pf = match shellexpand::full(local_path) {
            Ok(x) => PathBuf::from(x.as_ref()),
            Err(_) => panic!("not a valid local path"),
        };
        if local_path_pf.is_relative() {
            let mut current_dir = env::current_dir().unwrap();
            current_dir.push(local_path_pf);
            local_path_pf = current_dir;
        }
        let remote_path_pf: PathBuf = match remote_path {
            Some(x) => PathBuf::from(shellexpand::tilde_with_context(&x, || remote_home).as_ref()),
            None => {
                let mut pf = PathBuf::from(&remote_home.unwrap());
                match diff_paths(&local_path_pf, env!("HOME")) {
                    Some(x) => {
                        pf.push(x);
                    }
                    None => panic!("don't support upload to remote path other than home"),
                }
                pf
            }
        };
        Connection {
            remote_path: remote_path_pf,
            local_path: local_path_pf,
        }
    }

    fn calculate_remote_path(&self, p: &Path) -> PathBuf {
        let mut pf: PathBuf = PathBuf::from(&self.remote_path);
        pf.push(diff_paths(p, &self.local_path).unwrap());
        pf
    }
}

#[derive(PartialEq)]
enum Direction {
    Upload,
    Download,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut arg_iter = env::args().skip(1);
    let arg1: String = arg_iter.next().unwrap();
    let arg2: String = arg_iter.next().unwrap();
    if arg_iter.next().is_some() {
        panic!("expect 2 arguments, meet more than 2");
    }
    let arg1_split = arg1.split_once(':');
    let arg2_split = arg2.split_once(':');

    let (remote_host, remote_path, local_path, direction) = match (arg1_split, arg2_split) {
        (None, None) => {
            // scp local_path remote-host
            // ok
            (arg2, None, arg1, Direction::Upload)
        }
        (None, Some((remote_host, remote_path))) => {
            // scp local_path remote-host:remote-path
            // ok
            (
                remote_host.to_owned(),
                Some(remote_path),
                arg1,
                Direction::Upload,
            )
        }
        (Some((remote_host, remote_path)), None) => {
            // scp remote-host:remote-path local_path
            // ok
            (
                remote_host.to_owned(),
                Some(remote_path),
                arg2,
                Direction::Download,
            )
        }
        (Some(_), Some(_)) => {
            unimplemented!("don't support filename contains :")
        }
    };

    let (host_params, sess) = get_remote_host(&remote_host).await.unwrap();

    let sftp = Sftp::from_session(sess, SftpOptions::new()).await.unwrap();

    let mut connection = Connection::new(
        remote_path,
        &local_path,
        host_params.user.map(|u| format!("/home/{u}")),
    );

    connection.remote_path = sftp
        .fs()
        .canonicalize(connection.remote_path)
        .await
        .unwrap();

    match direction {
        Direction::Upload => {
            let _ = upload::upload(connection, &sftp).await;
            let _ = sftp.open("/tmp").await.unwrap().sync_all().await;
            Ok(())
        }
        Direction::Download => {
            let (_, sess) = get_remote_host(&remote_host).await.unwrap();
            let _ = download::download(connection, sess, sftp).await;
            File::open("/tmp").await.unwrap().sync_all().await.unwrap();
            Ok(())
        }
    }
}

fn is_gitignore_local(p: &Path) -> bool {
    let parent = {
        let mut x: PathBuf = PathBuf::from(p);
        x.pop();
        x
    };
    let output = Command::new("git")
        .args(["check-ignore", p.to_str().unwrap()])
        .current_dir(parent)
        .output()
        .expect("failed to git check-ignore");
    !output.stdout.is_empty()
}

async fn get_ignored_and_untracked(
    sess: &mut Session,
    dir: &Path,
) -> Result<HashSet<String>, Error> {
    let ls = sess
        .command("sh")
        .arg("-c")
        .arg(&format!("cd {} && git ls-files --others -z", dir.display()))
        .output()
        .await?;
    let s = String::from_utf8(ls.stdout).expect("server output was not valid UTF-8");
    let x: HashSet<_> = s
        .split(|x| x == '\0')
        .filter(|x| !x.is_empty())
        .map(|x| x.to_owned())
        .collect();
    Ok(x)
}

async fn get_untracked(sess: &mut Session, dir: &Path) -> Result<Vec<String>, Error> {
    let ls = sess
        .command("sh")
        .arg("-c")
        .arg(&format!(
            "cd {} && git ls-files --others --exclude-standard -z",
            dir.display(),
        ))
        .output()
        .await?;

    let s = String::from_utf8(ls.stdout).expect("server output was not valid UTF-8");
    let x: Vec<_> = s
        .split(|x| x == '\0')
        .filter(|x| !x.is_empty())
        .map(|x| x.to_owned())
        .collect();
    Ok(x)
}

async fn get_remote_host(remote_host: &str) -> Result<(HostParams, Session), openssh::Error> {
    let param = match remote_host.split_once(|x| x == '@') {
        Some((user_name, ip)) => HostParams {
            bind_address: None,
            bind_interface: None,
            ca_signature_algorithms: None,
            certificate_file: None,
            ciphers: None,
            compression: None,
            connection_attempts: None,
            connect_timeout: None,
            host_key_algorithms: None,
            host_name: Some(ip.to_owned()),
            identity_file: None,
            ignore_unknown: None,
            kex_algorithms: None,
            mac: None,
            port: None,
            pubkey_accepted_algorithms: None,
            pubkey_authentication: None,
            remote_forward: None,
            server_alive_interval: None,
            tcp_keep_alive: None,
            user: Some(user_name.to_owned()),
            ignored_fields: HashMap::new(),
        },
        None => {
            let ssh_config_location: PathBuf = [env!("HOME"), ".ssh", "config"].iter().collect();

            let mut reader = BufReader::new(
                std::fs::File::open(ssh_config_location)
                    .expect("Could not open configuration file"),
            );
            let config = SshConfig::default()
                .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS)
                .expect("Failed to parse configuration");

            // Query attributes for a certain host
            config.query(remote_host)
        }
    };
    match get_ssh_session(param.clone()).await {
        Ok(session) => Ok((param, session)),
        Err(_) => {
            let user = get_user_by_uid(get_current_uid()).unwrap();
            let h = HostParams {
                bind_address: None,
                bind_interface: None,
                ca_signature_algorithms: None,
                certificate_file: None,
                ciphers: None,
                compression: None,
                connection_attempts: None,
                connect_timeout: None,
                host_key_algorithms: None,
                host_name: Some(remote_host.to_owned()),
                identity_file: None,
                ignore_unknown: None,
                kex_algorithms: None,
                mac: None,
                port: None,
                pubkey_accepted_algorithms: None,
                pubkey_authentication: None,
                remote_forward: None,
                server_alive_interval: None,
                tcp_keep_alive: None,
                user: Some(user.name().to_str().unwrap().to_owned()),
                ignored_fields: HashMap::new(),
            };
            get_ssh_session(h.clone()).await.map(|session| (h, session))
        }
    }
}

async fn get_ssh_session(param: HostParams) -> Result<Session, openssh::Error> {
    Session::connect_mux(
        format!(
            "ssh://{}@{}:{}",
            param.user.unwrap(),
            param.host_name.unwrap(),
            param.port.unwrap_or(22)
        ),
        KnownHosts::Strict,
    )
    .await
}

/// Try to increase NOFILE limit and return the current soft limit.
pub fn increase_nofile_limit() -> std::io::Result<u64> {
    const DEFAULT_NOFILE_LIMIT: u64 = 16384; // or another number

    let (_soft, hard) = Resource::NOFILE.get()?;

    let target = cmp::min(DEFAULT_NOFILE_LIMIT, hard);
    Resource::NOFILE.set(target, hard)?;

    let (soft, _hard) = Resource::NOFILE.get()?;
    Ok(soft)
}
