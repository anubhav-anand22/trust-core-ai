//! The pairing code: what the Host shows and a Worker types in.
//!
//! Six characters, shown as `K7Q-4M9` so it is easy to read out, from Crockford's
//! base32: letters and digits without I, L, O or U, so nothing reads as another
//! character. The first two are a public **tag**: the Host advertises it on the
//! LAN so a Worker finds the right Host when several are running. The last four
//! are the **secret**, about a million values. That would be weak as a plain
//! password, but it only ever feeds a PAKE (see [`crate::link`]), so each guess
//! needs a live handshake, and a [`PairingWindow`] allows five failures: about
//! a 1-in-200,000 chance for someone guessing.

use std::time::{Duration, Instant};

use rand::Rng;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const TAG_LEN: usize = 2;
const SECRET_LEN: usize = 4;

/// MESH-13: a code lives 10 minutes and survives 5 failed attempts.
pub const DEFAULT_TTL: Duration = Duration::from_secs(10 * 60);
pub const DEFAULT_MAX_FAILURES: u8 = 5;

#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode {
    tag: String,
    secret: String,
}

impl PairingCode {
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let mut pick = |n: usize| -> String {
            (0..n).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect()
        };
        Self { tag: pick(TAG_LEN), secret: pick(SECRET_LEN) }
    }

    /// Accepts what a person types: any case, any dashes or spaces, and the
    /// usual look-alikes (O → 0, I/L → 1).
    pub fn parse(input: &str) -> Option<Self> {
        let mut chars = String::with_capacity(TAG_LEN + SECRET_LEN);
        for c in input.chars() {
            if c == '-' || c.is_whitespace() {
                continue;
            }
            let c = match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            if !ALPHABET.contains(&(c as u8)) || !c.is_ascii() {
                return None;
            }
            chars.push(c);
        }
        if chars.len() != TAG_LEN + SECRET_LEN {
            return None;
        }
        let (tag, secret) = chars.split_at(TAG_LEN);
        Some(Self { tag: tag.to_string(), secret: secret.to_string() })
    }

    /// Public. Advertised on the LAN; never enough to pair.
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Only ever fed to the PAKE. Never logged, never sent.
    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }
}

/// Two groups of three, whatever the tag/secret split: that is how people read it out.
impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let all = format!("{}{}", self.tag, self.secret);
        write!(f, "{}-{}", &all[..3], &all[3..])
    }
}

/// Never print the secret by accident (`?code` in a log line).
impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PairingCode({}*-***)", self.tag)
    }
}

/// The Host's current code, how long it has left, and how many failures it can take.
///
/// Any number of Workers may pair with one code while it is open; each still
/// needs the Host user's consent before it gets work.
#[derive(Debug)]
pub struct PairingWindow {
    code: PairingCode,
    opened: Instant,
    ttl: Duration,
    failures_left: u8,
    /// Attempts between [`Self::begin`] and [`Self::finish`]. Each one holds a
    /// unit of the failure budget while it runs: checking only at the start and
    /// counting only at the end would let 1,000 simultaneous connections make
    /// 1,000 guesses before the first failure was recorded.
    in_flight: u8,
}

/// Why a pairing attempt was refused before any key exchange.
#[derive(Debug, PartialEq, Eq)]
pub enum Closed {
    Expired,
    TooManyFailures,
    /// Every remaining attempt is already in use by other connections.
    Busy,
}

impl Closed {
    pub fn reason(&self) -> &'static str {
        match self {
            Closed::Expired => "this pairing code has expired; ask for a new one",
            Closed::TooManyFailures => "too many wrong attempts; ask for a new code",
            Closed::Busy => "other devices are pairing right now; try again in a moment",
        }
    }
}

impl PairingWindow {
    pub fn open(code: PairingCode, ttl: Duration, max_failures: u8) -> Self {
        Self { code, opened: Instant::now(), ttl, failures_left: max_failures, in_flight: 0 }
    }

    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    pub fn remaining(&self) -> Duration {
        self.ttl.saturating_sub(self.opened.elapsed())
    }

    /// Is the window still accepting attempts?
    pub fn check(&self) -> Result<(), Closed> {
        if self.failures_left == 0 {
            Err(Closed::TooManyFailures)
        } else if self.opened.elapsed() >= self.ttl {
            Err(Closed::Expired)
        } else {
            Ok(())
        }
    }

    /// Start an attempt: it holds one unit of the failure budget until
    /// [`Self::finish`]. Refused when the window is closed or every remaining
    /// unit is already held.
    pub fn begin(&mut self) -> Result<(), Closed> {
        self.check()?;
        if self.in_flight >= self.failures_left {
            return Err(Closed::Busy);
        }
        self.in_flight += 1;
        Ok(())
    }

    /// End an attempt started with [`Self::begin`]. Anything but a completed
    /// pairing (wrong code, dropped connection, timeout) spends the unit.
    pub fn finish(&mut self, paired: bool) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if !paired {
            self.failures_left = self.failures_left.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_code_round_trips_through_what_a_person_types() {
        let code = PairingCode::generate();
        let shown = code.to_string();
        assert_eq!(shown.len(), 7, "K7Q-4M9");
        assert!(shown.chars().all(|c| c == '-' || c.is_ascii_alphanumeric()));
        assert_eq!(PairingCode::parse(&shown).unwrap(), code);
        assert_eq!(PairingCode::parse(&shown.to_lowercase().replace('-', " ")).unwrap(), code);
        assert_eq!(PairingCode::parse(&shown.replace('-', "")).unwrap(), code, "the dash is optional");
    }

    #[test]
    fn look_alikes_are_forgiven_and_junk_is_refused() {
        assert_eq!(PairingCode::parse("ab0-1c1").unwrap(), PairingCode::parse("ABO-ICL").unwrap());
        assert!(PairingCode::parse("ABC-DE").is_none(), "too short");
        assert!(PairingCode::parse("ABC-DEFG").is_none(), "too long");
        assert!(PairingCode::parse("ABC-DE!").is_none());
        assert!(PairingCode::parse("ABC-DEU").is_none(), "U is not in the alphabet");
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let code = PairingCode::parse("K7Q-4M9").unwrap();
        assert_eq!(code.tag(), "K7");
        assert_eq!(format!("{code:?}"), "PairingCode(K7*-***)");
    }

    #[test]
    fn a_window_closes_on_time_or_after_the_failure_budget() {
        let mut w = PairingWindow::open(PairingCode::generate(), DEFAULT_TTL, 2);
        assert_eq!(w.begin(), Ok(()));
        w.finish(false);
        assert_eq!(w.check(), Ok(()));
        assert_eq!(w.begin(), Ok(()));
        w.finish(false);
        assert_eq!(w.check(), Err(Closed::TooManyFailures));
        assert_eq!(w.begin(), Err(Closed::TooManyFailures));

        let expired = PairingWindow::open(PairingCode::generate(), Duration::ZERO, 5);
        assert_eq!(expired.check(), Err(Closed::Expired));
    }

    #[test]
    fn simultaneous_attempts_cannot_exceed_the_budget() {
        let mut w = PairingWindow::open(PairingCode::generate(), DEFAULT_TTL, 3);
        assert!((0..3).all(|_| w.begin().is_ok()));
        assert_eq!(w.begin(), Err(Closed::Busy), "a 4th guess while 3 are running");
        w.finish(true); // a real device paired: its unit comes back
        assert_eq!(w.begin(), Ok(()));
        (0..3).for_each(|_| w.finish(false));
        assert_eq!(w.begin(), Err(Closed::TooManyFailures));
    }
}
