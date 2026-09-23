# Signing keys

`dev.pem` is the **development signing key: it is public, it is in the repository, and it protects
nothing.** It exists so that the whole path — sign an image, have the hardware verify it, stage an
update, commit it — is exercisable by anyone who can build this source, on a board that is either
open or carries this key's hash.

A device that matters is given a **production key, which never appears here**: it lives in the
release pipeline's secret store, and only its public half — and the hash of that, which is what the
hardware holds — is reproducible from this repository.

- Generate a key (secp256k1, the curve the boot facility verifies):
  `openssl ecparam -name secp256k1 -genkey -noout -out <name>.pem`
- The build signs with `dev.pem` unless it is told otherwise; the release build is told otherwise.
- Never give a production board the development key, and never copy a production key to a bench.
