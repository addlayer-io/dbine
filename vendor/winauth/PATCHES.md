# Changes from winauth 0.0.5

winauth 0.0.5 is the last published version and the crate is no longer
maintained. SQL Server's NTLMv2 sign-in depends on it, directly and through
tiberius.

- `rand` 0.7 -> 0.8.6 or later (GHSA-cq8v-f236-94qc). The code is unchanged: it
  only calls `thread_rng().fill_bytes`, for the NTLM client challenge and the
  session key.
- Security fixes found by DBine's 1.5.0 scan, in `src/lib.rs`:
  - The client wrote each AV_PAIR's and each AUTHENTICATE field's length from
    its UTF-8 byte count but the value in UTF-16, so a server's non-ASCII name
    made the length cover the client's own MIC flag and SPN pairs. Lengths are
    the UTF-16 byte count now, and a field too long for 16 bits is an error.
  - The CHALLENGE's payload offsets came from the server unchecked: their
    difference sized a zeroed allocation of up to about 4 GiB, and could
    underflow. The bytes up to a field are skipped without allocating, and an
    offset behind the cursor or past the message is an error.
  - An `MsvAvSingleHost` pair panicked (`unimplemented!()`); it's skipped by
    its length. Flags, timestamp and channel bindings must have their fixed
    length.
