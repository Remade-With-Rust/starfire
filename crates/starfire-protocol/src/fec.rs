// SPDX-License-Identifier: Apache-2.0
//! Video Reed-Solomon FEC, shared by the client (recovery) and the host
//! (parity generation) — docs/protocol/07-video-rtp-fec.md §2.
//!
//! The code on the wire is a **systematic Cauchy** code over GF(2^8) (primitive
//! polynomial `0x11d`): data shards `0..k`, then parity shards `k..k+m`, with
//! parity row `j`, data column `i` weighted by `1 / ((m + i) XOR j)`. That is
//! the matrix a Sunshine host emits (confirmed byte-for-byte against the
//! captured parity in `tests/fixtures/video/stream-hevc.fix`), so anything that
//! recovers with it interoperates with Sunshine and Comet alike.
//!
//! Two implementations live here on purpose:
//! * [`Fec`] — the shipping path. The matrix above is handed to
//!   `rusty_erasure`, whose kernels are SIMD (GFNI / AVX2 / SSSE3 / NEON) and
//!   whose decode inverts only what the loss pattern needs.
//! * [`scalar`] — the original first-party table-lookup implementation, kept
//!   forever as the **oracle**: every `Fec` result is gated byte-identical
//!   against it (see the tests), and it is the fallback if a coder cannot be
//!   built.

use std::collections::HashMap;

use rusty_erasure::{Coder, Matrix};

/// Largest `data + parity` shard count one FEC block can carry. The Cauchy
/// coefficient `(m + i) XOR j` must stay a nonzero byte, and Sunshine's encoder
/// caps a block at 255 shards.
pub const MAX_BLOCK_SHARDS: usize = 255;

/// Largest shard index the 10-bit `fecInfo` fields can express; the ceiling for
/// a block that carries **no** parity (nothing to invert, so no field limit).
pub const MAX_UNPROTECTED_SHARDS: usize = 1023;

/// Parity shards for `data_shards` at `pct` overhead: `ceil(k * pct / 100)`.
/// The host puts the final `pct` on the wire, so the client derives `m` from
/// `data_shards` and `pct` alone.
pub fn parity_count(data_shards: usize, pct: u8) -> usize {
    (data_shards * pct as usize).div_ceil(100)
}

/// The Cauchy coefficient for parity row `j`, data column `i` with `m` parity
/// shards: `1 / ((m + i) XOR j)` in GF(2^8).
fn parity_coeff(parity_shards: usize, i: usize, j: usize) -> u8 {
    rusty_erasure::gf::inv(((parity_shards + i) ^ j) as u8)
}

/// The full systematic generator `[I_k ; Cauchy]` as a `(k + m) x k` matrix.
fn generator(k: usize, m: usize) -> Option<Matrix> {
    if k == 0 || m == 0 || k + m > MAX_BLOCK_SHARDS {
        return None;
    }
    let mut bytes = vec![0u8; (k + m) * k];
    for i in 0..k {
        bytes[i * k + i] = 1;
    }
    for j in 0..m {
        let row = &mut bytes[(k + j) * k..(k + j + 1) * k];
        for (i, slot) in row.iter_mut().enumerate() {
            *slot = parity_coeff(m, i, j);
        }
    }
    Matrix::from_bytes(k + m, k, bytes).ok()
}

/// How many `(k, m)` coders to keep. A coder holds `m * k * 32` bytes of
/// expanded tables (up to ~350 KB at the block limit), and a steady stream
/// reuses a handful of shapes, so a small cache is both enough and bounded.
const CODER_CACHE: usize = 48;

/// A reusable FEC engine. Building a coder expands per-coefficient tables, so
/// keep one `Fec` per stream (the depacketizer and the packetizer each own
/// one) rather than constructing it per frame.
#[derive(Default)]
pub struct Fec {
    coders: HashMap<(u16, u16), Coder>,
    /// Blocks recovered through the SIMD path / through the scalar fallback —
    /// the reach census: the fallback count must stay zero in production.
    pub fast_blocks: u64,
    pub fallback_blocks: u64,
}

impl std::fmt::Debug for Fec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fec")
            .field("cached_coders", &self.coders.len())
            .field("fast_blocks", &self.fast_blocks)
            .field("fallback_blocks", &self.fallback_blocks)
            .finish()
    }
}

impl Fec {
    pub fn new() -> Self {
        Self::default()
    }

    /// The name of the kernel set production bytes flow through on this CPU
    /// (`"gfni"`, `"avx2"`, `"neon"`, `"scalar"`, ...). For reporting.
    pub fn kernel_name() -> &'static str {
        rusty_erasure::best_kernels().name
    }

    fn coder(&mut self, k: usize, m: usize) -> Option<&Coder> {
        let key = (k as u16, m as u16);
        if !self.coders.contains_key(&key) {
            let coder = rusty_erasure::coder(generator(k, m)?).ok()?;
            if self.coders.len() >= CODER_CACHE {
                self.coders.clear();
            }
            self.coders.insert(key, coder);
        }
        self.coders.get(&key)
    }

    /// Generate parity for one block: `data` is `k` equal-length shards,
    /// `parity` is `m` buffers of that same length (overwritten). Returns
    /// `false` (parity untouched) if the geometry is not encodable — more than
    /// [`MAX_BLOCK_SHARDS`] in total, an empty side, or mismatched lengths.
    pub fn encode(&mut self, data: &[&[u8]], parity: &mut [&mut [u8]]) -> bool {
        let (k, m) = (data.len(), parity.len());
        match self.coder(k, m) {
            Some(coder) => {
                let ok = coder.encode(data, parity).is_ok();
                if ok {
                    self.fast_blocks += 1;
                }
                ok
            }
            None => false,
        }
    }

    /// Recover the missing **data** shards of one block in place. `shards` has
    /// `data_shards + parity_shards` slots (`None` = lost); every present shard
    /// must be the same length. Returns `true` when all data shards are present
    /// afterwards, `false` if fewer than `data_shards` shards survive.
    ///
    /// Only runs on loss — the clean path never calls it.
    pub fn recover(
        &mut self,
        data_shards: usize,
        parity_shards: usize,
        shards: &mut [Option<Vec<u8>>],
    ) -> bool {
        let (k, m) = (data_shards, parity_shards);
        if shards.len() < k + m {
            return false;
        }
        let missing: Vec<usize> = (0..k).filter(|&i| shards[i].is_none()).collect();
        if missing.is_empty() {
            return true;
        }
        if shards[..k + m].iter().filter(|s| s.is_some()).count() < k {
            return false;
        }
        let Some(len) = shards.iter().flatten().map(|s| s.len()).next() else {
            return false;
        };
        if shards[..k + m].iter().flatten().any(|s| s.len() != len) {
            return false; // a malformed (short) shard cannot be mixed into the solve
        }

        let rebuilt = match self.coder(k, m) {
            Some(coder) => {
                let view: Vec<Option<&[u8]>> =
                    shards[..k + m].iter().map(|s| s.as_deref()).collect();
                let mut out: Vec<Vec<u8>> = vec![vec![0u8; len]; missing.len()];
                let ok = {
                    let mut out_refs: Vec<&mut [u8]> =
                        out.iter_mut().map(|v| v.as_mut_slice()).collect();
                    coder.recover(&view, &missing, &mut out_refs).is_ok()
                };
                ok.then_some(out)
            }
            None => None,
        };
        match rebuilt {
            Some(out) => {
                for (idx, buf) in missing.into_iter().zip(out) {
                    shards[idx] = Some(buf);
                }
                self.fast_blocks += 1;
                true
            }
            None => {
                self.fallback_blocks += 1;
                scalar::recover(k, m, shards)
            }
        }
    }
}

/// The original first-party scalar implementation: log/exp-table GF(2^8)
/// multiply, full Gauss-Jordan inverse. Slow (quadratic in bitrate) but small
/// and obviously correct — it stays as the oracle [`Fec`] is gated against and
/// as the fallback path.
pub mod scalar {
    use std::sync::OnceLock;

    /// GF(2^8) with primitive polynomial `0x11d` and generator `2`.
    struct Gf256 {
        /// `exp[i] = g^i` (doubled to 512 so `log[a]+log[b]` never wraps).
        exp: [u8; 512],
        log: [u8; 256],
        /// Multiplicative inverse table (`inv[0]` unused).
        inv: [u8; 256],
    }

    impl Gf256 {
        fn build() -> Self {
            let mut exp = [0u8; 512];
            let mut log = [0u8; 256];
            let mut x: u16 = 1;
            for (i, slot) in exp.iter_mut().take(255).enumerate() {
                *slot = x as u8;
                log[x as usize] = i as u8;
                x <<= 1;
                if x & 0x100 != 0 {
                    x ^= 0x11d;
                }
            }
            for i in 255..512 {
                exp[i] = exp[i - 255];
            }
            let mut inv = [0u8; 256];
            for a in 1..256usize {
                inv[a] = exp[255 - log[a] as usize];
            }
            Self { exp, log, inv }
        }

        #[inline]
        fn mul(&self, a: u8, b: u8) -> u8 {
            if a == 0 || b == 0 {
                0
            } else {
                self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
            }
        }

        #[inline]
        fn inverse(&self, a: u8) -> u8 {
            self.inv[a as usize]
        }
    }

    fn gf() -> &'static Gf256 {
        static GF: OnceLock<Gf256> = OnceLock::new();
        GF.get_or_init(Gf256::build)
    }

    fn parity_coeff(parity_shards: usize, i: usize, j: usize) -> u8 {
        gf().inverse(((parity_shards + i) ^ j) as u8)
    }

    /// Generate `parity_shards` parity shards from `data` (each `shard_len`
    /// bytes): parity row `j` = sum over `i` of `coeff(m, i, j) * data[i]`.
    pub fn encode_parity(data: &[&[u8]], parity_shards: usize, shard_len: usize) -> Vec<Vec<u8>> {
        let g = gf();
        let mut parity = vec![vec![0u8; shard_len]; parity_shards];
        for (j, out) in parity.iter_mut().enumerate() {
            for (i, src) in data.iter().enumerate() {
                let coeff = parity_coeff(parity_shards, i, j);
                if coeff == 0 {
                    continue;
                }
                for (o, &s) in out.iter_mut().zip(src.iter()) {
                    *o ^= g.mul(coeff, s);
                }
            }
        }
        parity
    }

    /// Invert a `k x k` GF(2^8) matrix (row-major) in place via Gauss-Jordan.
    fn invert(m: &mut [u8], k: usize) -> bool {
        let g = gf();
        let mut inv = vec![0u8; k * k];
        for i in 0..k {
            inv[i * k + i] = 1;
        }
        for col in 0..k {
            let mut piv = col;
            while piv < k && m[piv * k + col] == 0 {
                piv += 1;
            }
            if piv == k {
                return false; // singular
            }
            if piv != col {
                for c in 0..k {
                    m.swap(piv * k + c, col * k + c);
                    inv.swap(piv * k + c, col * k + c);
                }
            }
            let pvi = g.inverse(m[col * k + col]);
            for c in 0..k {
                m[col * k + c] = g.mul(m[col * k + c], pvi);
                inv[col * k + c] = g.mul(inv[col * k + c], pvi);
            }
            for r in 0..k {
                if r == col {
                    continue;
                }
                let f = m[r * k + col];
                if f == 0 {
                    continue;
                }
                for c in 0..k {
                    m[r * k + c] ^= g.mul(f, m[col * k + c]);
                    inv[r * k + c] ^= g.mul(f, inv[col * k + c]);
                }
            }
        }
        m.copy_from_slice(&inv);
        true
    }

    /// Recover missing data shards in one block from the parity shards. Same
    /// contract as [`super::Fec::recover`].
    pub fn recover(
        data_shards: usize,
        parity_shards: usize,
        shards: &mut [Option<Vec<u8>>],
    ) -> bool {
        let k = data_shards;
        if shards.len() < k + parity_shards || shards.iter().filter(|s| s.is_some()).count() < k {
            return false;
        }
        if (0..k).all(|i| shards[i].is_some()) {
            return true;
        }
        let len = match shards.iter().flatten().next() {
            Some(s) => s.len(),
            None => return false,
        };

        // The k surviving rows of the systematic generator F = [I_k ; Cauchy].
        let rows: Vec<usize> = shards
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_some())
            .map(|(i, _)| i)
            .take(k)
            .collect();
        let present: Vec<Vec<u8>> = rows.iter().filter_map(|&r| shards[r].clone()).collect();

        let mut mat = vec![0u8; k * k];
        for (r, &sh) in rows.iter().enumerate() {
            if sh < k {
                mat[r * k + sh] = 1;
            } else {
                let j = sh - k;
                for (c, slot) in mat[r * k..r * k + k].iter_mut().enumerate() {
                    *slot = parity_coeff(parity_shards, c, j);
                }
            }
        }
        if !invert(&mut mat, k) {
            return false;
        }

        let g = gf();
        for d in 0..k {
            if shards[d].is_some() {
                continue;
            }
            let mut out = vec![0u8; len];
            for (r, src) in present.iter().enumerate() {
                let coeff = mat[d * k + r];
                if coeff == 0 {
                    continue;
                }
                for (o, &s) in out.iter_mut().zip(src.iter()) {
                    *o ^= g.mul(coeff, s);
                }
            }
            shards[d] = Some(out);
        }
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Deterministic, non-trivial shard contents (no all-zero columns, so a
    /// wrong coefficient cannot hide behind `0 * x == 0`).
    fn shards(k: usize, len: usize, seed: u32) -> Vec<Vec<u8>> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        (0..k)
            .map(|_| {
                (0..len)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 17;
                        x ^= x << 5;
                        (x >> 8) as u8
                    })
                    .collect()
            })
            .collect()
    }

    fn encode_fast(fec: &mut Fec, data: &[Vec<u8>], m: usize, len: usize) -> Vec<Vec<u8>> {
        let refs: Vec<&[u8]> = data.iter().map(|d| d.as_slice()).collect();
        let mut parity = vec![vec![0u8; len]; m];
        let ok = {
            let mut pr: Vec<&mut [u8]> = parity.iter_mut().map(|p| p.as_mut_slice()).collect();
            fec.encode(&refs, &mut pr)
        };
        assert!(ok, "encode({}, {m}) should succeed", data.len());
        parity
    }

    /// The gate: the SIMD path must produce byte-identical parity to the scalar
    /// oracle across the geometry range, including both ends of a block.
    #[test]
    fn fast_parity_is_byte_identical_to_the_scalar_oracle() {
        let mut fec = Fec::new();
        // (k, m): tiny, typical, odd lengths, and the 255-shard block limit.
        for &(k, m, len) in &[
            (1usize, 1usize, 7usize),
            (3, 2, 33),
            (13, 3, 1360),
            (35, 7, 1376),
            (77, 39, 1360),
            (212, 43, 1360),
            (128, 127, 64),
            (254, 1, 64),
        ] {
            let data = shards(k, len, (k * 31 + m) as u32);
            let refs: Vec<&[u8]> = data.iter().map(|d| d.as_slice()).collect();
            let oracle = scalar::encode_parity(&refs, m, len);
            let fast = encode_fast(&mut fec, &data, m, len);
            assert_eq!(fast, oracle, "parity mismatch at k={k} m={m} len={len}");
        }
        assert_eq!(fec.fallback_blocks, 0);
    }

    /// Recovery through the fast path must equal the original data for every
    /// loss count up to `m`, wherever in the block the losses fall, and agree
    /// with the scalar oracle.
    #[test]
    fn fast_recovery_restores_exact_bytes_for_every_loss_count() {
        let mut fec = Fec::new();
        for &(k, m, len) in &[
            (5usize, 2usize, 33usize),
            (35, 7, 1376),
            (77, 16, 1360),
            (200, 55, 128),
        ] {
            let data = shards(k, len, (k + 7 * m) as u32);
            let parity = encode_fast(&mut fec, &data, m, len);
            let full: Vec<Option<Vec<u8>>> = data
                .iter()
                .cloned()
                .chain(parity.iter().cloned())
                .map(Some)
                .collect();
            for lost in 1..=m {
                // Spread the losses, always including shard 0 (it carries the
                // frame header) and the last data shard.
                let mut drop: Vec<usize> = (0..lost).map(|d| d * (k - 1) / lost.max(1)).collect();
                if lost >= 2 {
                    *drop.last_mut().unwrap() = k - 1;
                }
                drop.dedup();
                let mut a = full.clone();
                let mut b = full.clone();
                for &d in &drop {
                    a[d] = None;
                    b[d] = None;
                }
                assert!(
                    fec.recover(k, m, &mut a),
                    "fast recover k={k} m={m} lost={lost}"
                );
                assert!(
                    scalar::recover(k, m, &mut b),
                    "scalar recover k={k} m={m} lost={lost}"
                );
                for i in 0..k {
                    assert_eq!(
                        a[i].as_ref().unwrap(),
                        &data[i],
                        "fast: shard {i} (k={k} m={m} lost={lost})"
                    );
                    assert_eq!(b[i].as_ref().unwrap(), &data[i], "scalar: shard {i}");
                }
            }
        }
        assert_eq!(
            fec.fallback_blocks, 0,
            "recovery must stay on the fast path"
        );
    }

    /// One loss more than the parity can cover is unrecoverable, and must be
    /// reported as such rather than returning wrong bytes.
    #[test]
    fn too_many_losses_is_refused() {
        let mut fec = Fec::new();
        let (k, m, len) = (10usize, 3usize, 40usize);
        let data = shards(k, len, 99);
        let parity = encode_fast(&mut fec, &data, m, len);
        let mut s: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .chain(parity.iter().cloned())
            .map(Some)
            .collect();
        for slot in s.iter_mut().take(m + 1) {
            *slot = None; // m + 1 data shards gone
        }
        assert!(!fec.recover(k, m, &mut s));
        assert!(s[0].is_none(), "nothing may be written on failure");
    }

    /// Parity shards may be the lost ones; with all data present there is
    /// nothing to do and the data must be left untouched.
    #[test]
    fn lost_parity_only_is_a_no_op() {
        let mut fec = Fec::new();
        let (k, m, len) = (6usize, 3usize, 50usize);
        let data = shards(k, len, 5);
        let parity = encode_fast(&mut fec, &data, m, len);
        let mut s: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .chain(parity.iter().cloned())
            .map(Some)
            .collect();
        s[k] = None;
        s[k + 2] = None;
        assert!(fec.recover(k, m, &mut s));
        for i in 0..k {
            assert_eq!(s[i].as_ref().unwrap(), &data[i]);
        }
    }

    /// A block cannot exceed 255 shards: encode refuses (so the packetizer must
    /// split into more blocks) instead of emitting parity nobody can invert.
    #[test]
    fn oversize_block_is_refused_not_mis_encoded() {
        let mut fec = Fec::new();
        let (k, m, len) = (250usize, 6usize, 16usize); // 256 > MAX_BLOCK_SHARDS
        assert!(k + m > MAX_BLOCK_SHARDS);
        let data = shards(k, len, 1);
        let refs: Vec<&[u8]> = data.iter().map(|d| d.as_slice()).collect();
        let mut parity = vec![vec![0xAAu8; len]; m];
        let mut pr: Vec<&mut [u8]> = parity.iter_mut().map(|p| p.as_mut_slice()).collect();
        assert!(!fec.encode(&refs, &mut pr));
        assert!(
            parity.iter().all(|p| p.iter().all(|&b| b == 0xAA)),
            "parity untouched"
        );
    }

    #[test]
    fn parity_count_rounds_up() {
        assert_eq!(parity_count(35, 20), 7);
        assert_eq!(parity_count(1, 20), 1);
        assert_eq!(parity_count(3, 20), 1);
        assert_eq!(parity_count(10, 0), 0);
        assert_eq!(parity_count(212, 20), 43);
    }

    /// A short shard among the survivors must not be solved with (it would
    /// silently produce a truncated, wrong reconstruction).
    #[test]
    fn mismatched_shard_lengths_are_refused() {
        let mut fec = Fec::new();
        let (k, m, len) = (4usize, 2usize, 32usize);
        let data = shards(k, len, 3);
        let parity = encode_fast(&mut fec, &data, m, len);
        let mut s: Vec<Option<Vec<u8>>> = data
            .iter()
            .cloned()
            .chain(parity.iter().cloned())
            .map(Some)
            .collect();
        s[1] = None;
        s[4].as_mut().unwrap().truncate(10);
        assert!(!fec.recover(k, m, &mut s));
    }
}
