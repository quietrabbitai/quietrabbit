#!/usr/bin/env python3
"""Build src-tauri/src/auth/password_blocklist.txt (items.id=777, decisions.id=858).

Reproducible: given the pinned source file named in
src-tauri/src/auth/password_blocklist.MANIFEST.md, this script produces the
embedded list byte for byte.

    python3 tools/build_password_blocklist.py SOURCE.txt OUTPUT.txt

Rules (they must match src-tauri/src/auth/password_policy.rs):
  - each source line is decoded as UTF-8 (undecodable lines are skipped and
    counted), with only the trailing CR/LF removed;
  - NFKC-normalised, then lowercased (the same two steps the server applies to
    a candidate before it looks it up);
  - kept only when it is at least MIN_LEN code points long: the minimum
    password length is 15, so a shorter entry can never match anything;
  - de-duplicated and sorted by code point (identical to UTF-8 byte order,
    which is the order Rust's binary search uses).

No network access. Standard library only.
"""
import hashlib
import sys
import unicodedata

MIN_LEN = 15


def main(src: str, dst: str) -> None:
    kept = set()
    total = skipped_bytes = 0
    with open(src, "rb") as f:
        for raw in f:
            total += 1
            try:
                line = raw.decode("utf-8")
            except UnicodeDecodeError:
                skipped_bytes += 1
                continue
            line = line.rstrip("\r\n")
            entry = unicodedata.normalize("NFKC", line).lower()
            if len(entry) >= MIN_LEN and "\n" not in entry and "\r" not in entry:
                kept.add(entry)
    out = "".join(e + "\n" for e in sorted(kept))
    with open(dst, "w", encoding="utf-8", newline="\n") as f:
        f.write(out)
    data = out.encode("utf-8")
    print(f"source lines:        {total}")
    print(f"not valid UTF-8:     {skipped_bytes}")
    print(f"entries kept:        {len(kept)}")
    print(f"output bytes:        {len(data)}")
    print(f"output sha256:       {hashlib.sha256(data).hexdigest()}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
