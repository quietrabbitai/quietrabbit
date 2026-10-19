# password_blocklist.txt -- provenance (items.id=777, decisions.id=858)

Compiled into the binary with `include_str!` by `password_policy.rs`. No network
access at build or run time; the file is the whole list.

| | |
|---|---|
| Source | SecLists `Passwords/Common-Credentials/Pwdb_top-1000000.txt` |
| Source repo | https://github.com/danielmiessler/SecLists |
| Pinned commit | `60e43f4d2b0251330ff18ea4fa76aabec0515dcb` (committed 2026-10-18) |
| Source SHA-256 | `e9a88f67aafe65496682dc374559ee714e978bee50314767494c3e37a18c9fc8` |
| Source size | 8,755,171 bytes, 1,000,000 lines |
| Licence | SecLists is MIT (LICENSE and README read 2026-10-19; no exception named for password lists). The licence of the underlying breach-derived data is not stated by the repository (the Common-Credentials README names the Xato dataset and Probable-Wordlists as sources). Recorded, not resolved. |
| Generator | `tools/build_password_blocklist.py SOURCE OUTPUT` |
| Filter | NFKC, lowercase, keep only entries of 15 or more code points (the minimum password length, so shorter entries can never match), de-duplicate, sort by code point |
| Entries | 5,329 |
| Output size | 94,251 bytes |
| Output SHA-256 | `f305f8d7c39fdc57e3b32189e21e9efc9e75f5e9ef05c5584b111fb8b8efa4f9` (asserted by `password_policy::tests::blocklist_file_is_the_pinned_one`) |

Changing the list: re-run the generator against a new pinned source, then update
this file, `BLOCKLIST_SHA256` in `password_policy.rs`, and re-run the tests.

Honest scope: the list catches well-known long strings (it will not catch a
password nobody has leaked). The real defence against offline guessing is the
length rule plus the Argon2id cost (decisions.id=858, note of 2026-10-18).
