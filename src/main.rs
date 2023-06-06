use pathdiff::diff_paths;
use ssh2::Error;
use ssh2::Session;
use ssh2::Sftp;
use ssh2_config::HostParams;
use ssh2_config::SshConfig;
use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::io::BufReader;
use std::net::TcpStream;
use std::os::unix::prelude::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use users::{get_current_uid, get_user_by_uid};
use walkdir::WalkDir;

#[derive(Debug)]
struct Connection {
    remote_path: PathBuf,
    // always save an absolute path
    local_path: PathBuf,
}

impl Connection {
    fn new(remote_path: Option<&str>, local_path: &str, remote_home: String) -> Self {
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

fn main() -> Result<(), Error> {
    let mut arg_iter = env::args().skip(1);
    let arg1: String = arg_iter.next().unwrap();
    let arg2: String = arg_iter.next().unwrap();
    if arg_iter.next().is_some() {
        panic!("expect 2 arguments, meet more than 2");
    }
    let arg1_split = arg1.split_once(":");
    let arg2_split = arg2.split_once(":");

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

    let mut sess = get_remote_host(&remote_host)?;

    let remote_home_path = get_remote_home(&mut sess)?;
    let sftp = sess.sftp()?;

    let connection = Connection::new(remote_path, &local_path, remote_home_path);

    match direction {
        Direction::Upload => upload(connection, sess, sftp),
        Direction::Download => download(connection, sess, sftp),
    }
}

fn download(mut c: Connection, mut sess: Session, sftp: Sftp) -> Result<(), Error> {
    let remote_dir_filestat = sftp.stat(&c.remote_path).expect("can't access remote file");

    if remote_dir_filestat.is_dir() {
        if !c.local_path.exists() || c.local_path.is_dir() {
            if c.local_path.exists() {
                c.local_path.push(&c.remote_path.file_name().unwrap());
            }
            download_dir(&c, &mut sess, &sftp, &c.remote_path, None)
        } else {
            panic!("remote dir, local not dir");
        }
    } else {
        if c.local_path.is_dir() {
            c.local_path.push(&c.remote_path.file_name().unwrap());
        }
        download_file(&mut sess, &c.local_path, &c.remote_path)
    }
}

fn download_dir(
    c: &Connection,
    sess: &mut Session,
    sftp: &Sftp,
    remote_dir: &Path,
    blacklist: Option<HashSet<PathBuf>>,
) -> Result<(), Error> {
    // mkdir locally
    let mut local_dir = PathBuf::from(&c.local_path);
    local_dir.push(diff_paths(remote_dir, &c.remote_path).unwrap());

    let _ = std::fs::create_dir_all(&local_dir);

    let blacklist = match blacklist {
        Some(x) => x,
        None => {
            let mut ignored_or_untracked = get_ignored_and_untracked(sess, remote_dir)?;
            let untracked: Vec<String> = get_untracked(sess, remote_dir)?;
            untracked.into_iter().for_each(|x| {
                ignored_or_untracked.remove(&x);
            });
            ignored_or_untracked
                .into_iter()
                .map(|x| {
                    let mut pf = PathBuf::from(remote_dir);
                    pf.push(x);
                    pf
                })
                .collect()
        }
    };

    match sftp.readdir(remote_dir) {
        Ok(v) => {
            for (remote_pf, filestat) in v {
                if filestat.is_dir() {
                    download_dir(c, sess, &sftp, &remote_pf, Some(blacklist.clone()))?;
                } else {
                    if !blacklist.contains(&remote_pf) {
                        let mut local_path = local_dir.clone();
                        local_path.push(remote_pf.file_name().unwrap());
                        download_file(sess, &local_path, &remote_pf)?;
                    }
                }
            }
        }
        Err(_) => unreachable!(),
    }
    Ok(())
}

fn download_file(sess: &mut Session, local_path: &Path, remote_path: &Path) -> Result<(), Error> {
    println!("{:?}", remote_path.file_name().unwrap());
    let (mut remote_file, _stat) = sess.scp_recv(remote_path).unwrap();
    let mut contents = Vec::new();
    remote_file.read_to_end(&mut contents).unwrap();

    // Close the channel and wait for the whole content to be tranferred
    remote_file.send_eof()?;
    remote_file.wait_eof()?;
    remote_file.close()?;
    remote_file.wait_close()?;

    let mut local_file = File::create(local_path).unwrap();
    local_file.write(&contents).unwrap();
    Ok(())
}

fn upload(mut c: Connection, mut sess: Session, sftp: Sftp) -> Result<(), Error> {
    println!("upload {:?} ", c);
    let remote_dir_filestat = sftp.stat(&c.remote_path);

    if remote_dir_filestat.is_err() {
        let mut v: Vec<_> = c.remote_path.ancestors().skip(1).collect();
        v.reverse();
        for p in v {
            let _ = sftp.mkdir(p, 0o776);
        }
    }

    if c.local_path.is_dir() {
        match remote_dir_filestat {
            Ok(stat) => {
                if !stat.is_dir() {
                    panic!("remote path is not a dir");
                }
                c.remote_path.push(c.local_path.file_name().unwrap());
                let _ = sftp.mkdir(&c.remote_path, 0o776);
            }
            Err(_) => {
                let _ = sftp.mkdir(&c.remote_path, 0o776);
            }
        }

        let walker = WalkDir::new(&c.local_path).into_iter();
        for entry in walker {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                // ignore mkdir error
                let _ = sftp.mkdir(&c.calculate_remote_path(entry.path()), 0o776);
            } else {
                if !is_gitignore_local(entry.path()) {
                    let remote_path = c.calculate_remote_path(entry.path());
                    upload_file(&mut sess, entry.path(), &remote_path)?;
                }
            }
        }
    } else {
        match remote_dir_filestat {
            Ok(stat) => {
                if stat.is_dir() {
                    c.remote_path.push(c.local_path.file_name().unwrap());
                } else {
                }
            }
            Err(_) => {}
        }
        upload_file(&mut sess, &c.local_path, &c.remote_path)?;
    }
    Ok(())
}

fn upload_file(sess: &mut Session, local_path: &Path, remote_path: &Path) -> Result<(), Error> {
    println!("{:?}", local_path.file_name().unwrap());
    let mut file = File::open(local_path).unwrap();
    let permissions = file.metadata().unwrap().permissions().mode();

    let mut remote_file = sess.scp_send(
        &remote_path,
        (permissions & 0o777).try_into().unwrap(),
        file.metadata().unwrap().len(),
        None,
    )?;

    let mut v = vec![];
    file.read_to_end(&mut v).unwrap();

    remote_file.write_all(&v).unwrap();
    remote_file.flush().unwrap();
    // Close the channel and wait for the whole content to be transferred
    remote_file.send_eof()?;
    remote_file.wait_eof()?;
    remote_file.close()?;
    remote_file.wait_close()?;
    Ok(())
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

fn get_ignored_and_untracked(sess: &mut Session, dir: &Path) -> Result<HashSet<String>, Error> {
    let mut channel = sess.channel_session()?;
    channel.exec(&format!("cd {} && git ls-files --others -z", dir.display(),))?;
    let mut s = String::new();
    channel.read_to_string(&mut s).unwrap();
    channel.close()?;
    channel.wait_close()?;
    let x: HashSet<_> = s
        .split(|x| x == '\0')
        .map(|x| x.to_owned())
        .filter(|x| x.len() != 0)
        .collect();
    Ok(x)
}

fn get_untracked(sess: &mut Session, dir: &Path) -> Result<Vec<String>, Error> {
    let mut channel = sess.channel_session()?;
    channel.exec(&format!(
        "cd {} && git ls-files --others --exclude-standard -z",
        dir.display(),
    ))?;
    let mut s = String::new();
    channel.read_to_string(&mut s).unwrap();
    channel.close()?;
    channel.wait_close()?;
    let x: Vec<_> = s
        .split(|x| x == '\0')
        .map(|x| x.to_owned())
        .filter(|x| x.len() != 0)
        .collect();
    Ok(x)
}

fn get_remote_home(sess: &mut Session) -> Result<String, Error> {
    let mut channel = sess.channel_session()?;
    channel.exec("echo -n $HOME")?;
    let mut s = String::new();
    channel.read_to_string(&mut s).unwrap();
    channel.close()?;
    channel.wait_close()?;
    Ok(s)
}

fn get_remote_host(remote_host: &str) -> Result<Session, Error> {
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
                File::open(ssh_config_location).expect("Could not open configuration file"),
            );
            let config = SshConfig::default()
                .parse(&mut reader)
                .expect("Failed to parse configuration");

            // Query attributes for a certain host
            config.query(&remote_host)
        }
    };
    match get_ssh_session(param) {
        Ok(x) => {
            return Ok(x);
        }
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
            return get_ssh_session(h);
        }
    }
}

fn get_ssh_session(param: HostParams) -> Result<Session, Error> {
    let tcp = TcpStream::connect(format!(
        "{}:{}",
        param.host_name.unwrap(),
        param.port.unwrap_or(22)
    ))
    .unwrap();
    let mut sess = Session::new().unwrap();
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    // man ssh
    let default_identity_list: Vec<PathBuf> = vec![
        [env!("HOME"), ".ssh", "id_rsa"].iter().collect(),
        [env!("HOME"), ".ssh", "id_ecdsa"].iter().collect(),
        [env!("HOME"), ".ssh", "id_ecdsa_sk"].iter().collect(),
        [env!("HOME"), ".ssh", "id_ed25519"].iter().collect(),
        [env!("HOME"), ".ssh", "id_ed25519_sk"].iter().collect(),
        [env!("HOME"), ".ssh", "id_dsa"].iter().collect(),
    ];
    let v: Vec<PathBuf> = match param.identity_file {
        Some(mut identity_file) => {
            identity_file.extend(default_identity_list);
            identity_file
        }
        None => default_identity_list,
    };

    for identity_file in &v {
        match sess.userauth_pubkey_file(&param.user.clone().unwrap(), None, &identity_file, None) {
            Ok(_) => {
                if sess.authenticated() {
                    break;
                }
            }
            Err(_) => {}
        }
    }

    if !sess.authenticated() {
        panic!("authenticated failed")
    }
    Ok(sess)
}
