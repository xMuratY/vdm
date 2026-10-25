//! Expected file digests: parsing what users paste, and verifying downloads.

use std::fmt;
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

/// Block size used when streaming a file through a hasher.
const BLOCK_SIZE: usize = 1 << 20;

/// A hash algorithm VDM can verify a download with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl Algorithm {
    /// `"md5"`, `"sha1"`, `"sha256"` or `"sha512"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// Hex digits in a digest: 32, 40, 64 or 128.
    pub fn hex_len(self) -> usize {
        match self {
            Self::Md5 => 32,
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    /// Name as written in error messages.
    fn label(self) -> &'static str {
        match self {
            Self::Md5 => "MD5",
            Self::Sha1 => "SHA-1",
            Self::Sha256 => "SHA-256",
            Self::Sha512 => "SHA-512",
        }
    }

    /// Algorithm for a hex digest of `len` digits, if there is exactly one.
    fn from_hex_len(len: usize) -> Option<Self> {
        [Self::Md5, Self::Sha1, Self::Sha256, Self::Sha512].into_iter().find(|a| a.hex_len() == len)
    }

    /// Algorithm for a name such as `sha256` or `SHA-256`.
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "md5" => Some(Self::Md5),
            "sha1" | "sha-1" => Some(Self::Sha1),
            "sha256" | "sha-256" => Some(Self::Sha256),
            "sha512" | "sha-512" => Some(Self::Sha512),
            _ => None,
        }
    }
}

/// An expected digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Checksum {
    pub algorithm: Algorithm,
    /// Lowercase hex.
    pub hex: String,
}

/// A checksum string that could not be parsed. The message says why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ChecksumError(pub String);

fn error(message: impl Into<String>) -> ChecksumError {
    ChecksumError(message.into())
}

impl Checksum {
    /// Check `digest` has the right length and only hex digits for `algorithm`.
    fn with_algorithm(algorithm: Algorithm, digest: &str) -> Result<Self, ChecksumError> {
        if !is_hex(digest) {
            return Err(error(format!("{} checksum is not hexadecimal", algorithm.label())));
        }
        let want = algorithm.hex_len();
        if digest.len() != want {
            return Err(error(format!(
                "a {} checksum has {want} hex digits, this one has {}",
                algorithm.label(),
                digest.len()
            )));
        }
        Ok(Self { algorithm, hex: digest.to_ascii_lowercase() })
    }
}

impl FromStr for Checksum {
    type Err = ChecksumError;

    /// Accepts `algo:hex`, `algo=hex`, `algo hex`, bare hex (algorithm guessed
    /// from its length) and `sha256sum`/`md5sum` lines (`hex  file name`).
    /// Anything after the digest, such as a file name, is ignored.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let s = input.trim();
        if s.is_empty() {
            return Err(error("checksum is empty"));
        }

        // The first word, up to ':', '=' or whitespace.
        let (head, after) = match s.find(|c: char| c == ':' || c == '=' || c.is_whitespace()) {
            Some(i) => (&s[..i], &s[i..]),
            None => (s, ""),
        };
        let separator = after.chars().next();
        let rest = after.get(separator.map_or(0, char::len_utf8)..).unwrap_or("").trim();

        if let Some(algorithm) = Algorithm::from_name(head) {
            if rest.is_empty() {
                return Err(error(format!(
                    "{} checksum has no digest after its name",
                    algorithm.label()
                )));
            }
            return Self::with_algorithm(algorithm, first_word(rest));
        }
        if matches!(separator, Some(':' | '=')) {
            return Err(error(format!(
                "unknown checksum algorithm '{head}'; expected md5, sha1, sha256 or sha512"
            )));
        }

        // Bare hex, or a `sha256sum`-style line whose first word is the digest.
        let digest = first_word(s);
        if !is_hex(digest) {
            return Err(error(format!("checksum is not hexadecimal: '{digest}'")));
        }
        match Algorithm::from_hex_len(digest.len()) {
            Some(algorithm) => Self::with_algorithm(algorithm, digest),
            None => Err(error(format!(
                "can't tell the algorithm from a {}-digit checksum; prefix it, e.g. sha256:…",
                digest.len()
            ))),
        }
    }
}

impl fmt::Display for Checksum {
    /// `sha256:<hex>`, which parses back to the same checksum.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm.name(), self.hex)
    }
}

fn first_word(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("")
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Hash a file (streamed in 1 MiB blocks on a blocking thread). Returns lowercase hex.
pub async fn hash_file(path: &Path, algorithm: Algorithm) -> io::Result<String> {
    let path: PathBuf = path.to_path_buf();
    tokio::task::spawn_blocking(move || hash_file_blocking(&path, algorithm))
        .await
        .map_err(io::Error::other)?
}

/// Ok(true) if the file matches.
pub async fn verify(path: &Path, expected: &Checksum) -> io::Result<bool> {
    let actual = hash_file(path, expected.algorithm).await?;
    Ok(actual.eq_ignore_ascii_case(&expected.hex))
}

fn hash_file_blocking(path: &Path, algorithm: Algorithm) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; BLOCK_SIZE];
    match algorithm {
        Algorithm::Md5 => digest_reader::<Md5>(&mut file, &mut buf),
        Algorithm::Sha1 => digest_reader::<Sha1>(&mut file, &mut buf),
        Algorithm::Sha256 => digest_reader::<Sha256>(&mut file, &mut buf),
        Algorithm::Sha512 => digest_reader::<Sha512>(&mut file, &mut buf),
    }
}

fn digest_reader<D: Digest>(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<String> {
    let mut hasher = D::new();
    loop {
        match reader.read(buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `data` to a fresh file in `dir` and return its path.
    fn write_file(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    fn hex_of<D: Digest>(data: &[u8]) -> String {
        D::digest(data).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn parse(s: &str) -> Checksum {
        s.parse().unwrap_or_else(|e| panic!("{s:?} should parse: {e}"))
    }

    fn parse_err(s: &str) -> String {
        match s.parse::<Checksum>() {
            Ok(c) => panic!("{s:?} should not parse, got {c}"),
            Err(e) => e.0,
        }
    }

    const MD5_EMPTY: &str = "d41d8cd98f00b204e9800998ecf8427e";
    const MD5_ABC: &str = "900150983cd24fb0d6963f7d28e17f72";
    const SHA1_EMPTY: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    const SHA1_ABC: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
    const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA512_EMPTY: &str = "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";
    const SHA512_ABC: &str = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";

    #[test]
    fn algorithm_names_and_lengths() {
        assert_eq!(Algorithm::Md5.name(), "md5");
        assert_eq!(Algorithm::Sha1.name(), "sha1");
        assert_eq!(Algorithm::Sha256.name(), "sha256");
        assert_eq!(Algorithm::Sha512.name(), "sha512");
        assert_eq!(Algorithm::Md5.hex_len(), 32);
        assert_eq!(Algorithm::Sha1.hex_len(), 40);
        assert_eq!(Algorithm::Sha256.hex_len(), 64);
        assert_eq!(Algorithm::Sha512.hex_len(), 128);
    }

    #[tokio::test]
    async fn known_vectors_for_empty_and_abc() {
        let dir = tempfile::tempdir().unwrap();
        let empty = write_file(&dir, "empty", b"");
        let abc = write_file(&dir, "abc", b"abc");
        let cases = [
            (Algorithm::Md5, MD5_EMPTY, MD5_ABC),
            (Algorithm::Sha1, SHA1_EMPTY, SHA1_ABC),
            (Algorithm::Sha256, SHA256_EMPTY, SHA256_ABC),
            (Algorithm::Sha512, SHA512_EMPTY, SHA512_ABC),
        ];
        for (algorithm, want_empty, want_abc) in cases {
            assert_eq!(
                hash_file(&empty, algorithm).await.unwrap(),
                want_empty,
                "{algorithm:?} empty"
            );
            assert_eq!(hash_file(&abc, algorithm).await.unwrap(), want_abc, "{algorithm:?} abc");
        }
    }

    #[tokio::test]
    async fn large_file_matches_one_shot_hash() {
        // Several 1 MiB blocks plus a partial last block.
        let data: Vec<u8> = (0..3 * 1024 * 1024 + 4321).map(|i: usize| (i % 251) as u8).collect();
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "pattern.bin", &data);
        assert_eq!(hash_file(&path, Algorithm::Md5).await.unwrap(), hex_of::<Md5>(&data));
        assert_eq!(hash_file(&path, Algorithm::Sha1).await.unwrap(), hex_of::<Sha1>(&data));
        assert_eq!(hash_file(&path, Algorithm::Sha256).await.unwrap(), hex_of::<Sha256>(&data));
        assert_eq!(hash_file(&path, Algorithm::Sha512).await.unwrap(), hex_of::<Sha512>(&data));
    }

    #[tokio::test]
    async fn missing_file_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.iso");
        let err = hash_file(&missing, Algorithm::Sha256).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        let expected = parse(SHA256_ABC);
        assert_eq!(verify(&missing, &expected).await.unwrap_err().kind(), ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn verify_true_and_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "abc.bin", b"abc");
        assert!(verify(&path, &parse(&format!("sha256:{SHA256_ABC}"))).await.unwrap());
        assert!(verify(&path, &parse(MD5_ABC)).await.unwrap());
        assert!(!verify(&path, &parse(&format!("sha256:{SHA256_EMPTY}"))).await.unwrap());
        assert!(!verify(&path, &parse(MD5_EMPTY)).await.unwrap());
    }

    #[tokio::test]
    async fn verify_ignores_case_of_a_hand_built_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(&dir, "abc.bin", b"abc");
        let upper = Checksum { algorithm: Algorithm::Md5, hex: MD5_ABC.to_ascii_uppercase() };
        assert!(verify(&path, &upper).await.unwrap());
    }

    #[test]
    fn parses_prefixed_forms() {
        for s in [
            format!("sha256:{SHA256_ABC}"),
            format!("sha256={SHA256_ABC}"),
            format!("sha256 {SHA256_ABC}"),
            format!("sha256   {SHA256_ABC}"),
            format!("  SHA-256: {SHA256_ABC}  "),
            format!("Sha256:{}", SHA256_ABC.to_ascii_uppercase()),
        ] {
            let c = parse(&s);
            assert_eq!(c.algorithm, Algorithm::Sha256, "{s}");
            assert_eq!(c.hex, SHA256_ABC, "{s}");
        }
        assert_eq!(parse(&format!("md5:{MD5_ABC}")).algorithm, Algorithm::Md5);
        assert_eq!(parse(&format!("sha1:{SHA1_ABC}")).algorithm, Algorithm::Sha1);
        assert_eq!(parse(&format!("sha-1={SHA1_ABC}")).algorithm, Algorithm::Sha1);
        assert_eq!(parse(&format!("sha-512 {SHA512_ABC}")).algorithm, Algorithm::Sha512);
    }

    #[test]
    fn parses_bare_hex_by_length() {
        assert_eq!(parse(MD5_ABC), Checksum { algorithm: Algorithm::Md5, hex: MD5_ABC.into() });
        assert_eq!(parse(SHA1_ABC), Checksum { algorithm: Algorithm::Sha1, hex: SHA1_ABC.into() });
        assert_eq!(
            parse(SHA256_ABC),
            Checksum { algorithm: Algorithm::Sha256, hex: SHA256_ABC.into() }
        );
        assert_eq!(
            parse(SHA512_ABC),
            Checksum { algorithm: Algorithm::Sha512, hex: SHA512_ABC.into() }
        );
    }

    #[test]
    fn parses_sha256sum_and_md5sum_lines() {
        let c = parse(&format!("{SHA256_ABC}  ubuntu.iso"));
        assert_eq!(c, Checksum { algorithm: Algorithm::Sha256, hex: SHA256_ABC.into() });
        let c = parse(&format!("{MD5_ABC} *setup.exe"));
        assert_eq!(c, Checksum { algorithm: Algorithm::Md5, hex: MD5_ABC.into() });
        let c = parse(&format!("sha256:{SHA256_ABC}  ubuntu.iso"));
        assert_eq!(c.hex, SHA256_ABC);
    }

    #[test]
    fn stores_hex_lowercase() {
        let c = parse(&SHA1_ABC.to_ascii_uppercase());
        assert_eq!(c.algorithm, Algorithm::Sha1);
        assert_eq!(c.hex, SHA1_ABC);
    }

    #[test]
    fn bare_hex_of_unknown_length_is_an_error() {
        assert_eq!(
            parse_err("abc"),
            "can't tell the algorithm from a 3-digit checksum; prefix it, e.g. sha256:…"
        );
        let len_99 = "a".repeat(99);
        assert!(parse_err(&len_99).contains("99-digit checksum"));
    }

    #[test]
    fn empty_input_is_an_error() {
        assert_eq!(parse_err(""), "checksum is empty");
        assert_eq!(parse_err("   \t "), "checksum is empty");
    }

    #[test]
    fn prefix_without_digest_is_an_error() {
        assert_eq!(parse_err("sha256:"), "SHA-256 checksum has no digest after its name");
        assert_eq!(parse_err("md5 "), "MD5 checksum has no digest after its name");
        assert_eq!(parse_err("sha512"), "SHA-512 checksum has no digest after its name");
    }

    #[test]
    fn non_hex_is_an_error() {
        assert_eq!(parse_err("sha256:abc"), "a SHA-256 checksum has 64 hex digits, this one has 3");
        assert_eq!(parse_err("sha256:zz"), "SHA-256 checksum is not hexadecimal");
        let bad = format!("sha256:{}g", &SHA256_ABC[..63]);
        assert_eq!(parse_err(&bad), "SHA-256 checksum is not hexadecimal");
        let bare = format!("{}z", &SHA1_ABC[..39]);
        assert_eq!(
            parse_err(&bare),
            "checksum is not hexadecimal: 'a9993e364706816aba3e25717850c26c9cd0d89z'"
        );
    }

    #[test]
    fn wrong_length_for_prefixed_algorithm_is_an_error() {
        let short = &SHA256_ABC[..63];
        assert_eq!(
            parse_err(&format!("sha256:{short}")),
            "a SHA-256 checksum has 64 hex digits, this one has 63"
        );
        let long = format!("{SHA256_ABC}0");
        assert_eq!(
            parse_err(&format!("sha256 {long}")),
            "a SHA-256 checksum has 64 hex digits, this one has 65"
        );
        assert_eq!(
            parse_err(&format!("md5={MD5_ABC}0")),
            "a MD5 checksum has 32 hex digits, this one has 33"
        );
    }

    #[test]
    fn unknown_algorithm_is_an_error() {
        let err = parse_err(&format!("sha384:{}", "a".repeat(96)));
        assert_eq!(
            err,
            "unknown checksum algorithm 'sha384'; expected md5, sha1, sha256 or sha512"
        );
        assert!(parse_err("crc32=deadbeef").contains("unknown checksum algorithm 'crc32'"));
    }

    #[test]
    fn display_round_trips() {
        for c in [
            Checksum { algorithm: Algorithm::Md5, hex: MD5_ABC.into() },
            Checksum { algorithm: Algorithm::Sha1, hex: SHA1_ABC.into() },
            Checksum { algorithm: Algorithm::Sha256, hex: SHA256_ABC.into() },
            Checksum { algorithm: Algorithm::Sha512, hex: SHA512_ABC.into() },
        ] {
            let text = c.to_string();
            assert_eq!(text, format!("{}:{}", c.algorithm.name(), c.hex));
            assert_eq!(parse(&text), c);
        }
    }
}
