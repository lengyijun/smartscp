# Smartscp

A replacement of scp, but auto skip git-ignored files

It's just a wrapper of sshfs and [xcp](https://github.com/tarka/xcp)

## Install
```
cargo install --git https://github.com/lengyijun/smartscp --branch sshfs
```

## Usage
```
smartscp remote-host:path local_path
smartscp local_path remote-host
smartscp local_path remote-host:remote-path
```

## Feature
1. respect git ignore 

git-ignored files will not be scped

2. auto fill-in the remote path
```
# auto complete the destination: `remote_host:~/.local/share`
smartscp ~/.local/share remote_host
```

3. no `-r` needed in transferring folder

4. support interactive password input, but not recommanded

## Not supported yet
1. filename contains ":"

## Notice
Not compatible with scp
Not compatible with the same parameters as SCP

## Q&A
### Q: why not use `rsync --exclude=`
A: rsync doesn't support complicated exclude rules

### Q: why not rewrite scp from bottom up ?
A: It's not a trival work

### Q: why transfer failed ?
A: remote readonly files exists

## Reference
[Why scp is bad and difference between scp and sftp](https://goteleport.com/blog/scp-familiar-simple-insecure-slow/#alternatives-to-scp)

