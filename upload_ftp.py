"""Upload 官网/ directory to InfinityFree FTP (ftpupload.net) via ftplib.
Credentials read from ftp.txt. Uploads every file to /htdocs/ (binary mode),
skipping already-identical sizes, and lists the remote dir at the end.
"""
import ftplib
import os
import sys

LOCAL_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "官网")
REMOTE_DIR = "/htdocs"


def read_cred(path):
    with open(path, encoding="utf-8") as f:
        lines = [ln.strip() for ln in f if ln.strip()]
    cred = {}
    i = 0
    while i + 1 < len(lines):
        cred[lines[i].lower()] = lines[i + 1]
        i += 2
    return cred


def main():
    cred = read_cred(os.path.join(os.path.dirname(os.path.abspath(__file__)), "ftp.txt"))
    host = cred["ftp hostname"]
    port = int(cred.get("ftp port (optional)", 21))
    user = cred["ftp username"]
    pwd = cred["ftp password"]

    ftp = ftplib.FTP()
    ftp.connect(host, port, timeout=30)
    ftp.login(user, pwd)
    print("login ok:", ftp.getwelcome())

    # 确保远程目录存在
    try:
        ftp.cwd(REMOTE_DIR)
    except ftplib.error_perm:
        ftp.mkd(REMOTE_DIR)
        ftp.cwd(REMOTE_DIR)
    print("remote cwd:", ftp.pwd())

    # 远程已有文件大小（用于跳过相同文件）
    remote_sizes = {}
    try:
        for line in ftp.mlsd():
            name, facts = line
            if facts.get("type") == "file":
                remote_sizes[name] = int(facts.get("size", 0) or 0)
    except Exception:
        remote_sizes = {}

    files = sorted(os.listdir(LOCAL_DIR))
    ok = 0
    skipped = 0
    for fn in files:
        local = os.path.join(LOCAL_DIR, fn)
        if not os.path.isfile(local):
            continue
        size = os.path.getsize(local)
        if remote_sizes.get(fn) == size:
            print(f"skip  {fn} ({size} B, unchanged)")
            skipped += 1
            continue
        with open(local, "rb") as fh:
            ftp.storbinary(f"STOR {fn}", fh)
        print(f"upload {fn} ({size} B)")
        ok += 1

    print(f"done: {ok} uploaded, {skipped} skipped")
    ftp.quit()
    return 0


if __name__ == "__main__":
    sys.exit(main())
