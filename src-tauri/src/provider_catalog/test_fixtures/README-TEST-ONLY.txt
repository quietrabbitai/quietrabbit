TEST-ONLY FIXTURES (items.id=713). Nothing here is a release artifact.

Two throwaway minisign key pairs (A and B) were generated in a temporary
directory on 2026-10-12 and the directory was deleted. Only the PUBLIC keys
(test_key_a.pub, test_key_b.pub) and the signed fixtures are kept. No secret key
exists in this repository or anywhere else, so these fixtures cannot be
re-signed: a changed fixture needs new throwaway keys, and the tests that read
them must be updated with the new public keys.

  provider-catalog.json                 a catalog body naming the neutral
                                        fixture rows the tests seed
  catalog-manifest.json                 pins the body by size and SHA-256
  catalog-manifest.json.minisig         signature by key A
  catalog-manifest.json.keyb.minisig    signature by key B (rotation test)

The production public key is a different thing and is not here
(provider_catalog::verify::TRUSTED_PUBLIC_KEYS).
