use crate::get_ignored_and_untracked;
use crate::get_untracked;
use crate::Connection;
use async_recursion::async_recursion;
use bytes::BytesMut;
use futures::future::join_all;
use futures::stream::StreamExt;
use openssh::Session;
use openssh_sftp_client::Error;
use openssh_sftp_client::Sftp;
use pathdiff::diff_paths;
use std::collections::HashSet;
use std::future::ready;
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

pub async fn download(mut c: Connection, mut sess: Session, sftp: Sftp) -> Result<(), Error> {
    let remote_dir_filestat = sftp
        .open(&*c.remote_path)
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
    let local_dir = c
        .local_path
        .join(diff_paths(&remote_dir, &*c.remote_path).unwrap());

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
                .map(|x| remote_dir.join(x))
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
            let remote_pf = remote_dir.join(entry.filename());

            if entry.file_type().unwrap().is_dir() {
                v1.push(remote_pf);
            } else if !blacklist.contains(&remote_pf) {
                v2.push(download_file(
                    sftp,
                    local_dir.join(remote_pf.file_name().unwrap()),
                    remote_pf.clone(),
                ));
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
