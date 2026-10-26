use std::fmt;

use argon2::{Algorithm, Argon2, Params, Version};
use subtle::ConstantTimeEq;

/// Shortest accepted PIN, in digits.
pub const MIN_PIN_LEN: usize = 4;
/// Longest accepted PIN, in digits.
pub const MAX_PIN_LEN: usize = 8;
/// Salt length in bytes.
pub const SALT_LEN: usize = 16;
/// Derived key length in bytes.
pub const DIGEST_LEN: usize = 32;

/// Maximum Argon2 memory cost accepted from callers and persisted PIN records (64 MiB).
pub const MAX_HASH_MEMORY_KIB: u32 = 65_536;
/// Maximum Argon2 passes accepted from callers and persisted PIN records.
pub const MAX_HASH_ITERATIONS: u32 = 4;
/// Maximum Argon2 lanes accepted from callers and persisted PIN records.
pub const MAX_HASH_PARALLELISM: u32 = 4;

const RECORD_ALGORITHM: &str = "argon2id";
const RECORD_VERSION: &str = "v=19";

/// Why a PIN operation failed. No variant carries PIN text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinError {
    /// The PIN is not 4 to 8 ASCII digits.
    Format,
    /// Hash parameters are outside the range Argon2 accepts.
    Params,
    /// The Argon2 computation failed.
    Hash,
    /// No system randomness was available for a salt.
    Random,
    /// A stored record could not be parsed.
    Record,
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Format => "a PIN must be 4 to 8 digits",
            Self::Params => "the PIN hash parameters are invalid",
            Self::Hash => "hashing the PIN failed",
            Self::Random => "no system randomness is available for the PIN salt",
            Self::Record => "the stored PIN record is malformed",
        })
    }
}

impl std::error::Error for PinError {}

/// A validated PIN. Digits only, 4 to 8 long. Not `Clone`, and `Debug` is redacted.
pub struct Pin(String);

impl Pin {
    pub fn parse(input: &str) -> Result<Self, PinError> {
        let valid_len = (MIN_PIN_LEN..=MAX_PIN_LEN).contains(&input.len());
        if valid_len && input.bytes().all(|b| b.is_ascii_digit()) {
            Ok(Self(input.to_owned()))
        } else {
            Err(PinError::Format)
        }
    }

    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pin(<redacted>)")
    }
}

/// Argon2id cost parameters. Stored in the record so a later change of defaults does not break
/// verification of an existing PIN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashParams {
    /// Memory cost in KiB.
    pub memory_kib: u32,
    /// Number of passes.
    pub iterations: u32,
    /// Degree of parallelism.
    pub parallelism: u32,
}

impl HashParams {
    /// Argon2id minimum from the OWASP password storage guidance: 19 MiB, 2 passes, 1 lane.
    pub const OWASP_MINIMUM: Self = Self {
        memory_kib: 19_456,
        iterations: 2,
        parallelism: 1,
    };

    /// Validates a parameter set against Argon2 limits and this crate's resource bounds.
    pub fn new(memory_kib: u32, iterations: u32, parallelism: u32) -> Result<Self, PinError> {
        let params = Self {
            memory_kib,
            iterations,
            parallelism,
        };
        params.to_argon2()?;
        Ok(params)
    }

    fn to_argon2(self) -> Result<Params, PinError> {
        if self.memory_kib > MAX_HASH_MEMORY_KIB
            || self.iterations > MAX_HASH_ITERATIONS
            || self.parallelism > MAX_HASH_PARALLELISM
        {
            return Err(PinError::Params);
        }
        Params::new(
            self.memory_kib,
            self.iterations,
            self.parallelism,
            Some(DIGEST_LEN),
        )
        .map_err(|_| PinError::Params)
    }
}

/// Returns `SALT_LEN` bytes from the operating system's random source.
pub fn generate_salt() -> Result<[u8; SALT_LEN], PinError> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|_| PinError::Random)?;
    Ok(salt)
}

fn derive(pin: &Pin, params: HashParams, salt: &[u8]) -> Result<[u8; DIGEST_LEN], PinError> {
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params.to_argon2()?);
    let mut out = [0u8; DIGEST_LEN];
    argon
        .hash_password_into(pin.as_bytes(), salt, &mut out)
        .map_err(|_| PinError::Hash)?;
    Ok(out)
}

/// A salted Argon2id hash of a PIN. Comparison is constant-time. `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredPin {
    params: HashParams,
    salt: [u8; SALT_LEN],
    digest: [u8; DIGEST_LEN],
}

impl StoredPin {
    /// Hashes `pin` with the given salt. Pass salt from [`generate_salt`] in production.
    pub fn create(pin: &Pin, params: HashParams, salt: [u8; SALT_LEN]) -> Result<Self, PinError> {
        let digest = derive(pin, params, &salt)?;
        Ok(Self {
            params,
            salt,
            digest,
        })
    }

    /// True if `attempt` hashes to the stored digest. The final comparison is constant-time.
    pub fn matches(&self, attempt: &Pin) -> Result<bool, PinError> {
        let digest = derive(attempt, self.params, &self.salt)?;
        Ok(bool::from(digest.as_slice().ct_eq(self.digest.as_slice())))
    }

    /// The single-line record for the settings file:
    /// `argon2id$v=19$m=<KiB>,t=<passes>,p=<lanes>$<salt hex>$<digest hex>`.
    pub fn to_record(&self) -> String {
        format!(
            "{RECORD_ALGORITHM}${RECORD_VERSION}$m={},t={},p={}${}${}",
            self.params.memory_kib,
            self.params.iterations,
            self.params.parallelism,
            to_hex(&self.salt),
            to_hex(&self.digest),
        )
    }

    /// Parses a record produced by [`to_record`](Self::to_record).
    pub fn from_record(record: &str) -> Result<Self, PinError> {
        let parts: Vec<&str> = record.split('$').collect();
        let [algorithm, version, costs, salt_hex, digest_hex] = parts.as_slice() else {
            return Err(PinError::Record);
        };
        if *algorithm != RECORD_ALGORITHM || *version != RECORD_VERSION {
            return Err(PinError::Record);
        }
        let params = parse_costs(costs)?;
        let salt = from_hex::<SALT_LEN>(salt_hex).ok_or(PinError::Record)?;
        let digest = from_hex::<DIGEST_LEN>(digest_hex).ok_or(PinError::Record)?;
        Ok(Self {
            params,
            salt,
            digest,
        })
    }

    /// The cost parameters this PIN was hashed with.
    pub fn params(&self) -> HashParams {
        self.params
    }
}

impl fmt::Debug for StoredPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoredPin(<redacted>)")
    }
}

fn parse_costs(costs: &str) -> Result<HashParams, PinError> {
    let fields: Vec<&str> = costs.split(',').collect();
    let [memory, iterations, parallelism] = fields.as_slice() else {
        return Err(PinError::Record);
    };
    let memory_kib = cost_field(memory, "m=")?;
    let iterations = cost_field(iterations, "t=")?;
    let parallelism = cost_field(parallelism, "p=")?;
    HashParams::new(memory_kib, iterations, parallelism).map_err(|_| PinError::Record)
}

fn cost_field(field: &str, key: &str) -> Result<u32, PinError> {
    field
        .strip_prefix(key)
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or(PinError::Record)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != 2 * N || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        let pair = hex.get(2 * index..2 * index + 2)?;
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_hash_params_struct_literals_cannot_bypass_resource_bounds() {
        let pin = Pin::parse("1234").expect("valid test PIN");
        for params in [
            HashParams {
                memory_kib: u32::MAX,
                iterations: 1,
                parallelism: 1,
            },
            HashParams {
                memory_kib: 8,
                iterations: u32::MAX,
                parallelism: 1,
            },
            HashParams {
                memory_kib: 8,
                iterations: 1,
                parallelism: u32::MAX,
            },
        ] {
            assert_eq!(
                StoredPin::create(&pin, params, [0; SALT_LEN]).err(),
                Some(PinError::Params)
            );
        }
    }
}
