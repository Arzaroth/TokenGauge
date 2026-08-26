# One symmetric fleet key, and no revocation

The sync contribution crosses storage the user does not control, so it is
encrypted from the first release. We chose a **single 32-byte symmetric key
shared by every device** (XChaCha20-Poly1305, key copied between machines the
way a Syncthing device id is) over per-device keypairs, because a one-person
fleet does not need key distribution and every mechanism that would give it real
revocation costs a flow that is larger than the feature.

## Consequences

**There is no revocation.** A lost or stolen machine can read fleet usage until
every remaining device is re-keyed by hand. The docs say this rather than
implying that "encrypted" means "revocable", and `--sync-rotate` is deliberately
not built in v1: the manual path (`--sync-init --sync-force`, `--sync-join` on each
machine, delete the old objects) is the honest shape of the operation.

Object names are `HMAC(sync_key, device_id)` so the storage holder cannot link
an object to a machine. The fleet's *size* is not hidden - one object per device
means they are countable however they are named - and write timing still leaks
working hours.

Possession of the key is the only authentication. A device cannot prove which
machine it is beyond holding the key, which is the right level for one person's
machines and the wrong level for a team.
