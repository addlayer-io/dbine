# Changes from winauth 0.0.5

winauth 0.0.5 is the last published version and the crate is no longer
maintained. SQL Server's NTLMv2 sign-in depends on it, directly and through
tiberius.

- `rand` 0.7 -> 0.8.6 or later (GHSA-cq8v-f236-94qc). The code is unchanged: it
  only calls `thread_rng().fill_bytes`, for the NTLM client challenge and the
  session key.
