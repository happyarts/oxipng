//! The Incremental strategy: each line gets the filter whose output adds the fewest bits to a
//! running deflate stream of the lines chosen before it.
//!
//! The chosen lines are parsed into literals and matches as a fast greedy deflate would (hash
//! chains over a 32 KiB window) and their symbols are counted for the current block. A candidate
//! line is parsed on top of that, and its score is the size of the block with it, coded with
//! Huffman codes built for that block. Like a streaming compressor, the parse of the chosen lines
//! stays a little behind, so a match may run from one line into the next; on narrow images that
//! is at most a few lines, so each candidate parses a bounded multiple of its own length.

use super::strategies::StrategyEvaluator;

const WINDOW: usize = 1 << 15;
const WINDOW_MASK: usize = WINDOW - 1;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
/// How far the parse of the chosen lines stays behind their end
const LOOKAHEAD: usize = MAX_MATCH + MIN_MATCH + 1;
/// ... but at most this many lines, so that each candidate parses a bounded multiple of its own
/// length. On images only a few bytes wide this judges with less context than a streaming
/// compressor would.
const LOOKAHEAD_LINES: usize = 4;
/// Matches reach back as far as zlib's (`MAX_DIST`)
const MAX_DISTANCE: usize = WINDOW - LOOKAHEAD;
const HASH_BITS: u32 = 15;
/// Positions are kept as 32 bits (half the cache of `usize`); only their distance back counts,
/// so they may wrap on images of more than 4 GiB
const NO_POSITION: u32 = u32::MAX;
/// How many earlier positions with the same hash are tried for a match in the chosen lines...
const MAX_CHAIN: usize = 128;
/// ... and in a candidate line, which only has to compare with the other candidates
const TRIAL_CHAIN: usize = 32;
/// A block ends after this many symbols
const BLOCK_SYMBOLS: u32 = 16383;

const LITLEN_CODES: usize = 286;
const END_OF_BLOCK: usize = 256;
const DISTANCE_CODES: usize = 30;
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// The order in which a dynamic block's header lists the code length code lengths
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Symbol counts of the current block
#[derive(Clone)]
struct Block {
    litlen: [u32; LITLEN_CODES],
    distance: [u32; DISTANCE_CODES],
    symbols: u32,
    extra_bits: u64,
}

impl Block {
    const fn new() -> Self {
        // Every block ends with one end-of-block symbol
        let mut litlen = [0; LITLEN_CODES];
        litlen[END_OF_BLOCK] = 1;
        Self {
            litlen,
            distance: [0; DISTANCE_CODES],
            symbols: 0,
            extra_bits: 0,
        }
    }

    const fn add_literal(&mut self, byte: u8) {
        self.litlen[byte as usize] += 1;
        self.symbols += 1;
    }

    fn add_match(&mut self, length: usize, distance: usize) {
        let l = LENGTH_BASE.partition_point(|&b| b as usize <= length) - 1;
        let d = DISTANCE_BASE.partition_point(|&b| b as usize <= distance) - 1;
        self.litlen[END_OF_BLOCK + 1 + l] += 1;
        self.distance[d] += 1;
        self.extra_bits += u64::from(LENGTH_EXTRA[l] + DISTANCE_EXTRA[d]);
        self.symbols += 1;
    }

    /// The size of the block in bits: the smaller of fixed and dynamic Huffman codes
    fn bits(&self, coder: &mut Coder) -> u64 {
        let sum = |counts: &[u32]| counts.iter().map(|&c| u64::from(c)).sum::<u64>();
        let coded = |counts: &[u32], lengths: &[u8]| {
            counts
                .iter()
                .zip(lengths)
                .map(|(&c, &l)| u64::from(c) * u64::from(l))
                .sum::<u64>()
        };
        // 3 bits block header; the fixed code's lengths by symbol range
        let litlen = &self.litlen;
        let fixed = 3
            + self.extra_bits
            + 8 * sum(&litlen[..144])
            + 9 * sum(&litlen[144..256])
            + 7 * sum(&litlen[256..280])
            + 8 * sum(&litlen[280..])
            + 5 * sum(&self.distance);

        // A dynamic block codes at least one distance
        let mut distance = self.distance;
        if distance.iter().all(|&count| count == 0) {
            distance[0] = 1;
        }
        let mut litlen_lengths = [0; LITLEN_CODES];
        let mut distance_lengths = [0; DISTANCE_CODES];
        coder.lengths(litlen, 15, &mut litlen_lengths);
        coder.lengths(&distance, 15, &mut distance_lengths);
        let dynamic = 3
            + self.extra_bits
            + coded(litlen, &litlen_lengths)
            + coded(&distance, &distance_lengths)
            + coder.header_bits(&litlen_lengths, &distance_lengths);
        dynamic.min(fixed)
    }
}

/// Builds Huffman code lengths, reusing its buffers
#[derive(Default)]
struct Coder {
    /// (count, node) of the symbols in use, by increasing count
    leaves: Vec<(u32, u16)>,
    /// (count, node) of the internal nodes in the order they are made
    internal: Vec<(u32, u16)>,
    parent: Vec<u16>,
    depth: Vec<u8>,
    /// Symbol of each leaf node
    symbol: Vec<u16>,
}

impl Coder {
    /// Huffman code lengths for `counts`, at most `max_bits` long
    fn lengths(&mut self, counts: &[u32], max_bits: u8, lengths: &mut [u8]) {
        lengths.fill(0);
        self.symbol.clear();
        self.leaves.clear();
        for (symbol, &count) in counts.iter().enumerate() {
            if count > 0 {
                self.leaves.push((count, self.symbol.len() as u16));
                self.symbol.push(symbol as u16);
            }
        }
        let n = self.leaves.len();
        if n < 2 {
            if let Some(&symbol) = self.symbol.first() {
                lengths[symbol as usize] = 1;
            }
            return;
        }
        self.leaves.sort_unstable();

        // Two queues: leaves by count, internal nodes in the order they are made (also by count)
        self.internal.clear();
        self.parent.clear();
        self.parent.resize(2 * n - 1, 0);
        let (mut next_leaf, mut next_internal) = (0, 0);
        for node in n..2 * n - 1 {
            let mut take = || {
                let leaf = self.leaves.get(next_leaf);
                let internal = self.internal.get(next_internal);
                match (leaf, internal) {
                    (Some(&leaf), Some(&internal)) if leaf.0 > internal.0 => {
                        next_internal += 1;
                        internal
                    }
                    (Some(&leaf), _) => {
                        next_leaf += 1;
                        leaf
                    }
                    (None, Some(&internal)) => {
                        next_internal += 1;
                        internal
                    }
                    (None, None) => unreachable!("a tree of n leaves has n - 1 internal nodes"),
                }
            };
            let (a, b) = (take(), take());
            self.parent[a.1 as usize] = node as u16;
            self.parent[b.1 as usize] = node as u16;
            self.internal.push((a.0 + b.0, node as u16));
        }

        // Each node's parent comes after it; the root is the last node
        self.depth.clear();
        self.depth.resize(2 * n - 1, 0);
        for node in (0..2 * n - 2).rev() {
            self.depth[node] = self.depth[self.parent[node] as usize] + 1;
        }
        let mut overflow = false;
        for leaf in 0..n {
            let depth = self.depth[leaf];
            overflow |= depth > max_bits;
            lengths[self.symbol[leaf] as usize] = depth.min(max_bits);
        }
        if overflow {
            // Shortened codes oversubscribe the code: lengthen the least frequent codes that
            // are still short until it fits again
            let kraft = |length: u8| 1_u64 << (max_bits - length);
            let mut sum: u64 = self
                .symbol
                .iter()
                .map(|&s| kraft(lengths[s as usize]))
                .sum();
            while sum > 1 << max_bits {
                let leaf = self
                    .leaves
                    .iter()
                    .map(|&(_, leaf)| self.symbol[leaf as usize] as usize)
                    .find(|&s| lengths[s] < max_bits)
                    .expect("a complete code fits in max_bits");
                sum -= kraft(lengths[leaf]) / 2;
                lengths[leaf] += 1;
            }
        }
    }

    /// Bits of a dynamic block's header for these code lengths: HLIT, HDIST, HCLEN, the code
    /// length code and the run-length coded lengths
    fn header_bits(&mut self, litlen: &[u8; LITLEN_CODES], distance: &[u8; DISTANCE_CODES]) -> u64 {
        let used_litlen = litlen
            .iter()
            .rposition(|&l| l > 0)
            .map_or(0, |p| p + 1)
            .max(257);
        let used_distance = distance
            .iter()
            .rposition(|&l| l > 0)
            .map_or(0, |p| p + 1)
            .max(1);
        let all = || {
            litlen[..used_litlen]
                .iter()
                .chain(&distance[..used_distance])
                .copied()
        };

        // Run-length code: 16 repeats the previous length 3-6 times (2 extra bits), 17 and 18
        // code 3-10 and 11-138 zeros (3 and 7 extra bits)
        let mut counts = [0; 19];
        let mut extra_bits = 0;
        let mut lengths = all().peekable();
        while let Some(length) = lengths.next() {
            let mut run = 1;
            while lengths.next_if_eq(&length).is_some() {
                run += 1;
            }
            if length == 0 {
                while run >= 11 {
                    counts[18] += 1;
                    extra_bits += 7;
                    run -= run.min(138);
                }
                if run >= 3 {
                    counts[17] += 1;
                    extra_bits += 3;
                    run = 0;
                }
            } else {
                counts[length as usize] += 1;
                run -= 1;
                while run >= 3 {
                    counts[16] += 1;
                    extra_bits += 2;
                    run -= run.min(6);
                }
            }
            counts[length as usize] += run as u32;
        }

        let mut code_lengths = [0; 19];
        self.lengths(&counts, 7, &mut code_lengths);
        let listed = CODE_LENGTH_ORDER
            .iter()
            .rposition(|&code| code_lengths[code] > 0)
            .map_or(0, |p| p + 1)
            .max(4);
        let coded: u64 = counts
            .iter()
            .zip(code_lengths)
            .map(|(&c, l)| u64::from(c) * u64::from(l))
            .sum();
        5 + 5 + 4 + 3 * listed as u64 + coded + extra_bits
    }
}

/// Hash chains over the positions parsed so far
struct Matcher {
    head: Vec<u32>,
    prev: Vec<u32>,
}

impl Matcher {
    fn new() -> Self {
        Self {
            head: vec![NO_POSITION; 1 << HASH_BITS],
            prev: vec![NO_POSITION; WINDOW],
        }
    }

    fn hash(data: &[u8], pos: usize) -> usize {
        let bytes =
            u32::from(data[pos]) | u32::from(data[pos + 1]) << 8 | u32::from(data[pos + 2]) << 16;
        (bytes.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    }

    /// Insert a position; with `undo`, record what it replaced
    fn insert(&mut self, data: &[u8], pos: usize, undo: Option<&mut Vec<Insertion>>) {
        if pos + MIN_MATCH > data.len() {
            return;
        }
        let hash = Self::hash(data, pos);
        if let Some(undo) = undo {
            undo.push(Insertion {
                hash,
                head: self.head[hash],
                prev: self.prev[pos & WINDOW_MASK],
            });
        }
        self.prev[pos & WINDOW_MASK] = self.head[hash];
        self.head[hash] = pos as u32;
    }

    /// Take back insertions, latest first
    fn undo(&mut self, insertions: &[Insertion]) {
        for insertion in insertions.iter().rev() {
            let pos = self.head[insertion.hash] as usize;
            self.prev[pos & WINDOW_MASK] = insertion.prev;
            self.head[insertion.hash] = insertion.head;
        }
    }

    /// The longest earlier match for the bytes at `pos` (at most to the end of `data`):
    /// (length, distance), or None if shorter than the minimum
    fn longest_match(&self, data: &[u8], pos: usize, max_chain: usize) -> Option<(usize, usize)> {
        let limit = (data.len() - pos).min(MAX_MATCH);
        if limit < MIN_MATCH {
            return None;
        }
        let target = &data[pos..pos + limit];
        // Candidates are valid as far back as the window reaches (and not before the start)
        let reach = pos.min(MAX_DISTANCE);
        let back = |candidate: u32| (pos as u32).wrapping_sub(candidate) as usize;
        let mut best = (MIN_MATCH - 1, 0);
        let mut candidate = self.head[Self::hash(data, pos)];
        let mut distance = back(candidate);
        let mut chain = max_chain;
        // Chains run to lower positions; anything else is a slot reused by a later position
        while candidate != NO_POSITION && distance > 0 && distance <= reach && chain > 0 {
            let c = pos - distance;
            if data[c + best.0] == target[best.0] {
                let length = common_prefix(&data[c..c + limit], target);
                if length > best.0 {
                    best = (length, distance);
                    if length == limit {
                        break;
                    }
                }
            }
            let next = self.prev[c & WINDOW_MASK];
            let next_distance = back(next);
            if next == NO_POSITION || next_distance <= distance {
                break;
            }
            candidate = next;
            distance = next_distance;
            chain -= 1;
        }
        (best.0 >= MIN_MATCH).then_some(best)
    }

    /// Parse `data` greedily from `from` until at least `to` into `block`, inserting every
    /// position (recorded in `undo` if given, for a candidate line, which searches less deeply);
    /// `full` gets each block that fills up before it starts anew. Returns where the parse
    /// stopped (a match may run past `to`).
    fn parse(
        &mut self,
        data: &[u8],
        from: usize,
        to: usize,
        block: &mut Block,
        mut undo: Option<&mut Vec<Insertion>>,
        mut full: impl FnMut(&Block),
    ) -> usize {
        let mut pos = from;
        while pos < to {
            let max_chain = if undo.is_some() {
                TRIAL_CHAIN
            } else {
                MAX_CHAIN
            };
            if let Some((length, distance)) = self.longest_match(data, pos, max_chain) {
                block.add_match(length, distance);
                for p in pos..pos + length {
                    self.insert(data, p, undo.as_deref_mut());
                }
                pos += length;
            } else {
                block.add_literal(data[pos]);
                self.insert(data, pos, undo.as_deref_mut());
                pos += 1;
            }
            if block.symbols >= BLOCK_SYMBOLS {
                full(block);
                *block = Block::new();
            }
        }
        pos
    }
}

/// How many leading bytes `a` and `b` (of equal length) have in common
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let mut length = 0;
    for (a, b) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0) {
        let diff = u64::from_le_bytes(*a) ^ u64::from_le_bytes(*b);
        if diff != 0 {
            return length + diff.trailing_zeros() as usize / 8;
        }
        length += 8;
    }
    length
        + a[length..]
            .iter()
            .zip(&b[length..])
            .take_while(|(a, b)| a == b)
            .count()
}

/// What an insertion into the hash chains replaced
#[derive(Clone, Copy)]
struct Insertion {
    hash: usize,
    head: u32,
    prev: u32,
}

pub(crate) struct IncrementalEvaluator {
    matcher: Matcher,
    /// The current block of the chosen lines
    block: Block,
    /// Where the parse of the chosen lines stopped
    parsed: usize,
    best_bits: u64,
    coder: Coder,
    undo: Vec<Insertion>,
}

impl IncrementalEvaluator {
    pub(crate) fn new() -> Self {
        Self {
            matcher: Matcher::new(),
            block: Block::new(),
            parsed: 0,
            best_bits: u64::MAX,
            coder: Coder::default(),
            undo: Vec::new(),
        }
    }
}

impl StrategyEvaluator for IncrementalEvaluator {
    fn reset(&mut self, _line_len: usize) {
        self.best_bits = u64::MAX;
    }

    fn look_back(&self, _line_len: usize) -> usize {
        // The matcher keeps positions in the whole output
        usize::MAX
    }

    fn evaluate(&mut self, output: &[u8], offset: usize) -> bool {
        // Bring the parse of the chosen lines up to the lookahead before this line. With a
        // lookahead shorter than a match, a match may end at the chosen lines' end; its last
        // positions can't be hashed yet and stay out of the chains (fewer match candidates later,
        // nothing wrong).
        let line_len = output.len() - offset;
        let lookahead = LOOKAHEAD.min(LOOKAHEAD_LINES * line_len);
        self.parsed = self.matcher.parse(
            &output[..offset],
            self.parsed,
            offset.saturating_sub(lookahead),
            &mut self.block,
            None,
            |_| {},
        );

        // Parse the rest with this line to the end, then take back its insertions
        let mut block = self.block.clone();
        let mut bits = 0;
        self.undo.clear();
        self.matcher.parse(
            output,
            self.parsed,
            output.len(),
            &mut block,
            Some(&mut self.undo),
            |full| bits += full.bits(&mut self.coder),
        );
        bits += block.bits(&mut self.coder);
        self.matcher.undo(&self.undo);

        if bits < self.best_bits {
            self.best_bits = bits;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lengths form a prefix code within the limit; returns them and the Kraft sum
    fn check(counts: &[u32], max_bits: u8) -> (Vec<u8>, u64) {
        let mut lengths = vec![0; counts.len()];
        Coder::default().lengths(counts, max_bits, &mut lengths);
        assert!(
            lengths
                .iter()
                .zip(counts)
                .all(|(&l, &c)| (l == 0) == (c == 0))
        );
        assert!(lengths.iter().all(|&l| l <= max_bits));
        let kraft: u64 = lengths
            .iter()
            .filter(|&&l| l > 0)
            .map(|&l| 1 << (max_bits - l))
            .sum();
        assert!(kraft <= 1 << max_bits);
        (lengths, kraft)
    }

    #[test]
    fn huffman_lengths() {
        assert_eq!(check(&[0, 5, 0], 15).0, [0, 1, 0]);
        assert_eq!(check(&[3, 1, 1], 15), (vec![1, 2, 2], 1 << 15));
        assert_eq!(check(&[1; LITLEN_CODES], 15).1, 1 << 15);
        // Fibonacci counts make the deepest tree; past the limit the code stays a prefix code
        let mut fib = vec![1_u32, 1];
        while fib.len() < 30 {
            fib.push(fib[fib.len() - 1] + fib[fib.len() - 2]);
        }
        check(&fib, 15);
        check(&fib[..19], 7);
    }

    #[test]
    fn match_codes() {
        let mut block = Block::new();
        block.add_match(258, 32768);
        assert_eq!(block.litlen[285], 1);
        assert_eq!(block.distance[29], 1);
        assert_eq!(block.extra_bits, 13);
        block.add_match(3, 1);
        assert_eq!(block.litlen[257], 1);
        assert_eq!(block.distance[0], 1);
        assert_eq!(block.extra_bits, 13);
    }

    #[test]
    fn block_sizes() {
        let mut coder = Coder::default();
        // An empty block: the fixed code's end-of-block (7 bits) after the 3-bit header
        assert_eq!(Block::new().bits(&mut coder), 10);
        // A few literals: fixed code, 8 bits each for bytes below 144, 9 above
        let mut block = Block::new();
        block.add_literal(0);
        block.add_literal(200);
        assert_eq!(block.bits(&mut coder), 10 + 8 + 9);
        // Many repeats of one long match: the dynamic code wins with short codes
        let mut block = Block::new();
        for _ in 0..1000 {
            block.add_match(258, 1);
        }
        let bits = block.bits(&mut coder);
        assert!(bits < 3 + 1000 * (8 + 5));
        assert!(bits >= 3 + 1000 * 2);
    }

    #[test]
    fn common_prefix_lengths() {
        let a: Vec<u8> = (0..40).collect();
        for n in 0..40 {
            let mut b = a.clone();
            b[n] ^= 1;
            assert_eq!(common_prefix(&a, &b), n);
        }
        assert_eq!(common_prefix(&a, &a), 40);
    }
}
