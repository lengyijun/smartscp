use async_recursion::async_recursion;
use bytes::BytesMut;
use futures::future::join_all;
use futures::stream::{self, StreamExt};
use openssh::{KnownHosts, Session};
use openssh_sftp_client::metadata::Permissions;
use openssh_sftp_client::Error;
use openssh_sftp_client::{Sftp, SftpOptions};
use pathdiff::diff_paths;
use ssh2_config::HostParams;
use ssh2_config::SshConfig;
use std::collections::HashSet;
use std::env;
use std::future::ready;
use std::io::BufReader;
use std::os::unix::prelude::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use users::{get_current_uid, get_user_by_uid};
use walkdir::{DirEntry, WalkDir};

#[derive(Debug)]
struct Connection {
    remote_path: PathBuf,
    // always save an absolute path
    local_path: PathBuf,
}

impl Connection {
    fn new(remote_path: Option<&str>, local_path: &str, remote_home: PathBuf) -> Self {
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
            Some(x) => PathBuf::from(
                shellexpand::tilde_with_context(&x, || Some(remote_home.clone())).as_ref(),
            ),

            None => {
                let mut pf = PathBuf::from(&remote_home);
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

    let sess = get_remote_host(&remote_host).await.unwrap();

    let sftp = Sftp::from_session(sess, SftpOptions::new()).await.unwrap();
    let remote_home_path = PathBuf::from("~");

    let mut connection = Connection::new(remote_path, &local_path, remote_home_path);
    connection.remote_path = sftp
        .fs()
        .canonicalize(connection.remote_path)
        .await
        .unwrap();

    match direction {
        Direction::Upload => {
            let _ = upload(connection, &sftp).await;
            let _ = sftp.open("/tmp").await.unwrap().sync_all().await;
            Ok(())
        }
        Direction::Download => {
            let sess = get_remote_host(&remote_host).await.unwrap();
            let _ = download(connection, sess, sftp).await;
            File::open("/tmp").await.unwrap().sync_all().await.unwrap();
            Ok(())
        }
    }
}

async fn download(mut c: Connection, mut sess: Session, sftp: Sftp) -> Result<(), Error> {
    let remote_dir_filestat = sftp
        .open(&c.remote_path)
        .await
        .unwrap()
        .metadata()
        .await
        .unwrap()
        .file_type()
        .unwrap();

    if remote_dir_filestat.is_dir() {
        if !c.local_path.exists() || c.local_path.is_dir() {
            if c.local_path.exists() {
                c.local_path.push(c.remote_path.file_name().unwrap());
            }
            download_dir(&c, &mut sess, &sftp, c.remote_path.clone(), None).await
        } else {
            panic!("remote dir, local not dir");
        }
    } else {
        if c.local_path.is_dir() {
            c.local_path.push(c.remote_path.file_name().unwrap());
        }
        download_file(&sftp, c.local_path.clone(), c.remote_path.clone()).await
    }
}

#[async_recursion]
async fn download_dir(
    c: &Connection,
    sess: &mut Session,
    sftp: &Sftp,
    remote_dir: PathBuf,
    blacklist: Option<HashSet<PathBuf>>,
) -> Result<(), Error> {
    // mkdir locally
    let mut local_dir = PathBuf::from(&c.local_path);
    local_dir.push(diff_paths(&remote_dir, &c.remote_path).unwrap());

    let _ = std::fs::create_dir_all(&local_dir);

    let blacklist = match blacklist {
        Some(x) => x,
        None => {
            let mut ignored_or_untracked =
                get_ignored_and_untracked(sess, &remote_dir).await.unwrap();
            let untracked: Vec<String> = get_untracked(sess, &remote_dir).await.unwrap();
            untracked.into_iter().for_each(|x| {
                ignored_or_untracked.remove(&x);
            });
            ignored_or_untracked
                .into_iter()
                .map(|x| {
                    let mut pf = PathBuf::from(&remote_dir);
                    pf.push(x);
                    pf
                })
                .collect()
        }
    };

    let mut v1 = vec![];
    let mut v2 = vec![];
    sftp.fs()
        .open_dir(&remote_dir)
        .await
        .unwrap()
        .read_dir()
        .for_each(|res| {
            let entry = res.unwrap();
            let filename = entry.filename().as_os_str();

            if filename == "." || filename == ".." {
                return ready(());
            }
            let mut remote_pf = PathBuf::from(&remote_dir);
            remote_pf.push(entry.filename());

            if entry.file_type().unwrap().is_dir() {
                v1.push(remote_pf);
            } else if !blacklist.contains(&remote_pf) {
                let mut local_path = local_dir.clone();
                local_path.push(remote_pf.file_name().unwrap());
                v2.push(download_file(sftp, local_path.clone(), remote_pf.clone()));
            }
            ready(())
        })
        .await;

    for remote_pf in v1.into_iter() {
        let _ = download_dir(c, sess, sftp, remote_pf.clone(), Some(blacklist.clone())).await;
    }
    join_all(v2).await;

    Ok(())
}

async fn download_file(
    sftp: &Sftp,
    local_path: PathBuf,
    remote_path: PathBuf,
) -> Result<(), Error> {
    println!("{:?}", remote_path.file_name().unwrap());
    let mut remote_file = sftp.open(remote_path).await?;
    let len: u64 = remote_file.metadata().await?.len().unwrap();

    let contents = remote_file.read_all(len as usize, BytesMut::new()).await?;

    let mut local_file = File::create(local_path).await.unwrap();
    local_file.write_all(&contents).await.unwrap();
    Ok(())
}

async fn upload(mut c: Connection, sftp: &Sftp) -> Result<(), Error> {
    println!("upload {:?} ", c);
    let remote_dir_filestat = sftp
        .open(&c.remote_path)
        .await
        .unwrap()
        .metadata()
        .await
        .map(|x| x.file_type());

    if remote_dir_filestat.is_err() {
        let mut v: Vec<_> = c.remote_path.ancestors().skip(1).collect();
        v.reverse();
        for p in v {
            sftp.fs().create_dir(p).await.unwrap();
        }
    }

    if c.local_path.is_dir() {
        match remote_dir_filestat {
            Ok(Some(stat)) => {
                if !stat.is_dir() {
                    panic!("remote path is not a dir");
                }
                c.remote_path.push(c.local_path.file_name().unwrap());
                // needless to create_dir here
                // we will create_dir in walker
                // let _ = sftp.fs().create_dir(&c.remote_path).await;
            }
            _ => {
                sftp.fs().create_dir(&c.remote_path).await.unwrap();
            }
        }

        let walker = WalkDir::new(&c.local_path).into_iter();
        let mut v = vec![];
        for entry in walker {
            let entry = entry.unwrap();
            v.push(upload_worker(&c, sftp, entry));
        }

        // send too fast will run out of fd
        let stream = stream::iter(v).buffered(1024).collect::<Vec<_>>().await;

        for x in stream.into_iter() {
            match x {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{:?}", e);
                }
            }
        }
        Ok(())
    } else {
        match remote_dir_filestat {
            Ok(Some(stat)) => {
                if stat.is_dir() {
                    c.remote_path.push(c.local_path.file_name().unwrap());
                } else {
                }
            }
            _ => {}
        }
        upload_file(sftp, &c.local_path, &c.remote_path)
            .await
            .map_err(|(e, _)| e)
    }
}

async fn upload_worker(
    c: &Connection,
    sftp: &Sftp,
    entry: DirEntry,
) -> Result<(), (Error, PathBuf)> {
    if entry.path().is_dir() {
        let _ = sftp
            .fs()
            .create_dir(&c.calculate_remote_path(entry.path()))
            .await;
        Ok(())
    } else if !is_gitignore_local(entry.path()) {
        let remote_path = c.calculate_remote_path(entry.path());
        upload_file(sftp, entry.path(), &remote_path).await
    } else {
        Ok(())
    }
}

async fn upload_file(
    sftp: &Sftp,
    local_path: &Path,
    remote_path: &Path,
) -> Result<(), (Error, PathBuf)> {
    println!("{:?}", local_path.file_name().unwrap());
    let mut file = File::open(local_path).await.unwrap();
    let permissions = file.metadata().await.unwrap().permissions().mode() & 0o777;

    let mut v = vec![];
    file.read_to_end(&mut v).await.unwrap();

    let mut f = sftp
        .options()
        .create(true)
        .write(true)
        .open(&remote_path)
        .await
        .map_err(|e| (e, PathBuf::from(remote_path)))?;
    let mut perm = Permissions::new();

    perm.set_read_by_owner((permissions & 0b100_000_000) != 0);
    perm.set_write_by_owner((permissions & 0b010_000_000) != 0);
    perm.set_execute_by_owner((permissions & 0b001_000_000) != 0);

    perm.set_read_by_group((permissions & 0b000_100_000) != 0);
    perm.set_write_by_group((permissions & 0b000_010_000) != 0);
    perm.set_execute_by_group((permissions & 0b000_001_000) != 0);

    perm.set_read_by_other((permissions & 0b000_000_100) != 0);
    perm.set_write_by_other((permissions & 0b000_000_010) != 0);
    perm.set_execute_by_other((permissions & 0b000_000_001) != 0);

    // write first, then set permission
    // permission maybe readonly
    f.write_all(&v)
        .await
        .map_err(|e| (e, PathBuf::from(remote_path)))?;

    f.set_permissions(perm)
        .await
        .map_err(|e| (e, PathBuf::from(remote_path)))
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
        .map(|x| x.to_owned())
        .filter(|x| !x.is_empty())
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

async fn get_remote_host(remote_host: &str) -> Result<Session, openssh::Error> {
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
        },
        None => {
            let ssh_config_location: PathBuf = [env!("HOME"), ".ssh", "config"].iter().collect();

            let mut reader = BufReader::new(
                std::fs::File::open(ssh_config_location)
                    .expect("Could not open configuration file"),
            );
            let config = SshConfig::default()
                .parse(&mut reader)
                .expect("Failed to parse configuration");

            // Query attributes for a certain host
            config.query(remote_host)
        }
    };
    match get_ssh_session(param).await {
        Ok(x) => Ok(x),
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
            };
            get_ssh_session(h).await
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
