// To regenerate tables, run the following in the repo root:
//
// $ cargo install ucd-generate
// $ curl -LO https://www.unicode.org/Public/18.0.0/ucd/UCD.zip
// $ unzip UCD.zip -d UCD
// $ ucd-generate property-bool UCD --include XID_Start,XID_Continue > tests/tables/tables.rs
// $ ucd-generate property-bool UCD --include XID_Start,XID_Continue --fst-dir tests/fst
// $ ucd-generate property-bool UCD --include XID_Start,XID_Continue --trie-set > tests/trie/trie.rs
// $ cargo run --manifest-path generate/Cargo.toml

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation, // https://github.com/rust-lang/rust-clippy/issues/9613
    clippy::items_after_statements,
    clippy::let_underscore_untyped,
    clippy::match_wild_err_arm,
    clippy::module_name_repetitions,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unwrap_or_default
)]

mod output;
mod parse;
mod write;

use crate::parse::parse_xid_properties;
use std::collections::BTreeMap as Map;
use std::collections::BTreeSet as Set;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process;

const CHUNK: usize = 64;
const UCD: &str = "UCD";
const TABLES: &str = "src/tables.rs";

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let unicode_ident_dir = manifest_dir.parent().unwrap();
    let ucd_dir = unicode_ident_dir.join(UCD);
    let properties = parse_xid_properties(&ucd_dir);

    let mut chunkmap = Map::<[u8; CHUNK], usize>::new();
    let mut dense = Vec::<[u8; CHUNK]>::new();
    let mut new_chunk = |chunk| {
        if let Some(prev) = chunkmap.get(&chunk) {
            *prev
        } else {
            let new = dense.len();
            dense.push(chunk);
            chunkmap.insert(chunk, new);
            new
        }
    };

    let empty_chunk = [0u8; CHUNK];
    let zero_chunk = new_chunk(empty_chunk);

    let mut trie_start = Vec::<usize>::new();
    let mut trie_continue = Vec::<usize>::new();
    for i in 0..(u32::from(char::MAX) + 1) / CHUNK as u32 / 8 {
        let mut start_bits = empty_chunk;
        let mut continue_bits = empty_chunk;
        for j in 0..CHUNK as u32 {
            let this_start = &mut start_bits[j as usize];
            let this_continue = &mut continue_bits[j as usize];
            for k in 0..8u32 {
                let code = (i * CHUNK as u32 + j) * 8 + k;
                if code >= 0x80 {
                    if let Some(ch) = char::from_u32(code) {
                        *this_start |= (properties.is_xid_start(ch) as u8) << k;
                        *this_continue |= (properties.is_xid_continue(ch) as u8) << k;
                    }
                }
            }
        }
        trie_start.push(new_chunk(start_bits));
        trie_continue.push(new_chunk(continue_bits));
    }

    while trie_start.last() == Some(&zero_chunk) {
        trie_start.pop();
    }
    while trie_continue.last() == Some(&zero_chunk) {
        trie_continue.pop();
    }

    // Lay out the LEAF array in two regions, one addressed by is_xid_start
    // relative to LEAF_START and the other addressed by is_xid_continue
    // relative to LEAF_CONTINUE. An 8-bit half-chunk index reaches 128.5 chunks
    // past its base address, so giving the two functions distinct base
    // addresses lets the array hold 128.5 chunks reachable by each function,
    // instead of 128.5 chunks in total.
    //
    //     LEAF_START     -> all-zero chunk
    //                       chunks reachable only by is_xid_start
    //     LEAF_CONTINUE  -> all-zero chunk
    //                       chunks reachable by both functions
    //                       chunks reachable only by is_xid_continue
    //
    // Every chunk of the second region must be reachable by is_xid_continue,
    // while only the front of it needs to remain reachable by is_xid_start, so
    // that is where the chunks reachable by both functions go. The all-zero
    // chunk, which the trie's entries for codepoints having no identifier
    // characters point to, begins both regions.

    let used_start = Set::from_iter(trie_start.iter().copied());
    let used_continue = Set::from_iter(trie_continue.iter().copied());

    // Contents of the two regions, apart from the all-zero chunk which begins
    // both of them.
    let start_region = Set::from_iter(used_start.difference(&used_continue).copied());
    let continue_region = Set::from_iter(
        used_continue
            .iter()
            .copied()
            .filter(|&chunk| chunk != zero_chunk),
    );
    let both = Set::from_iter(used_start.intersection(&used_continue).copied());

    let (mut leaf, start_layout) = compress(&dense, &start_region, zero_chunk, &Set::new());
    let leaf_continue = leaf.len();
    let (continue_halfdense, continue_layout) =
        compress(&dense, &continue_region, zero_chunk, &both);
    leaf.extend_from_slice(&continue_halfdense);

    // Position of each chunk relative to LEAF_START and LEAF_CONTINUE
    // respectively, in units of half-chunks.
    let continue_offset = leaf_continue / (CHUNK / 2);
    let trie_start: Vec<u8> = trie_start
        .iter()
        .map(|chunk| {
            let position = match start_layout.get(chunk) {
                Some(&position) => position,
                None => continue_offset + continue_layout[chunk],
            };
            u8::try_from(position).expect("exceeded 128.5 chunks reachable by is_xid_start")
        })
        .collect();
    let trie_continue: Vec<u8> = trie_continue
        .iter()
        .map(|chunk| {
            u8::try_from(continue_layout[chunk])
                .expect("exceeded 128.5 chunks reachable by is_xid_continue")
        })
        .collect();

    // Fallback for codepoints beyond the end of the trie.
    let zero_start = trie_start
        .iter()
        .position(|&i| i == 0)
        .expect("no all-zero chunk");
    let zero_continue = trie_continue
        .iter()
        .position(|&i| i == 0)
        .expect("no all-zero chunk");

    let out = write::output(
        &properties,
        zero_start,
        zero_continue,
        leaf_continue,
        &trie_start,
        &trie_continue,
        &leaf,
    );
    let path = unicode_ident_dir.join(TABLES);
    if let Err(err) = fs::write(&path, out) {
        let _ = writeln!(io::stderr(), "{}: {err}", path.display());
        process::exit(1);
    }
}

// Lay out one contiguous region of the LEAF array, compressed by overlapping
// chunks at half-chunk boundaries.
//
// If chunk i's back half equals chunk j's front half, placing them adjacently
// saves 32 bytes. We find the maximum number of such overlaps by modeling this
// as a bipartite matching problem (left side = back halves, right side = front
// halves) and solving with Kuhn's algorithm.
//
// Returns the bytes of the region, together with the half-chunk position of
// every laid out chunk relative to the beginning of the region. The chunk given
// as `first` is laid out at position 0, and chunks given in `prefer` are laid
// out as near to the front of the region as the chaining allows.
fn compress(
    dense: &[[u8; CHUNK]],
    group: &Set<usize>,
    first: usize,
    prefer: &Set<usize>,
) -> (Vec<u8>, Map<usize, usize>) {
    // Chunks of this region, identified below by their position in here rather
    // than by their index in `dense`.
    let mut members = vec![first];
    members.extend(group.iter().copied().filter(|&chunk| chunk != first));
    let num_chunks = members.len();

    let front_of: Vec<[u8; CHUNK / 2]> = members
        .iter()
        .map(|&chunk| dense[chunk][..CHUNK / 2].try_into().unwrap())
        .collect();
    let back_of: Vec<[u8; CHUNK / 2]> = members
        .iter()
        .map(|&chunk| dense[chunk][CHUNK / 2..].try_into().unwrap())
        .collect();

    // Build index from front-half value to chunk indices for efficient lookup.
    let mut chunks_by_front: Map<[u8; CHUNK / 2], Vec<usize>> = Map::new();
    for (j, &front) in front_of.iter().enumerate() {
        chunks_by_front
            .entry(front)
            .or_insert_with(Vec::new)
            .push(j);
    }

    // adj_list[i] = chunks whose front half matches chunk i's back half,
    // meaning they can follow chunk i with a 32-byte overlap. Exclude
    // self-edges (the all-zeros and all-ones chunks have front == back).
    let adj_list: Vec<Vec<usize>> = (0..num_chunks)
        .map(|i| {
            chunks_by_front
                .get(&back_of[i])
                .map_or_else(Vec::new, |js| {
                    js.iter().copied().filter(|&j| j != i).collect()
                })
        })
        .collect();

    // Maximum bipartite matching via Kuhn's algorithm (augmenting paths).
    // prev_of[j] = Some(i) means chunk i is matched to precede chunk j.
    let mut prev_of: Vec<Option<usize>> = vec![None; num_chunks];

    // DFS for an augmenting path from `src`. If found, augments the matching
    // in-place (rehoming existing matches to preserve validity) and returns
    // true.
    fn try_kuhn(
        src: usize,
        adj_list: &[Vec<usize>],
        visited: &mut [bool],
        prev_of: &mut [Option<usize>],
    ) -> bool {
        for &dst in &adj_list[src] {
            if !visited[dst] {
                visited[dst] = true;
                // If dst is free, or its current match can be rehomed, claim dst.
                if prev_of[dst].is_none_or(|prev| try_kuhn(prev, adj_list, visited, prev_of)) {
                    prev_of[dst] = Some(src);
                    return true;
                }
            }
        }
        false
    }

    // Try every left vertex. A failed attempt stays failed because later rounds
    // only shrink the set of free right vertices (Berge's theorem).
    for i in 0..num_chunks {
        let mut visited = vec![false; num_chunks];
        try_kuhn(i, &adj_list, &mut visited, &mut prev_of);
    }

    // Invert the matching into a forward map for chain traversal.
    let mut next_of: Vec<Option<usize>> = vec![None; num_chunks];
    for (j, &prev) in prev_of.iter().enumerate() {
        if let Some(prev) = prev {
            next_of[prev] = Some(j);
        }
    }

    // The chunk that must be laid out first cannot follow another chunk. Remove
    // any incoming edge so that it becomes a chain start.
    if let Some(prev) = prev_of[0].take() {
        next_of[prev] = None;
    }

    // Lay out chains, beginning with the one led by the chunk that must come
    // first.
    let mut leaf = Vec::<u8>::new();
    let mut layout = Map::<usize, usize>::new();

    fn lay_out_chain(
        start: usize,
        members: &[usize],
        front_of: &[[u8; CHUNK / 2]],
        back_of: &[[u8; CHUNK / 2]],
        next_of: &[Option<usize>],
        leaf: &mut Vec<u8>,
        layout: &mut Map<usize, usize>,
    ) {
        layout.insert(members[start], leaf.len() / (CHUNK / 2));
        leaf.extend_from_slice(&front_of[start]);
        leaf.extend_from_slice(&back_of[start]);

        // Write the rest of the chain: each chunk's front half overlaps the
        // previous chunk's back half, so only append the back half.
        let mut curr = start;
        while let Some(next) = next_of[curr] {
            layout.insert(members[next], leaf.len() / (CHUNK / 2) - 1);
            leaf.extend_from_slice(&back_of[next]);
            curr = next;
        }
    }

    // The chain led by the chunk that must come first goes first, followed by
    // the chains that contain a preferred chunk, followed by the rest.
    let mut chains: Vec<usize> = (0..num_chunks).filter(|&i| prev_of[i].is_none()).collect();
    chains.sort_by_key(|&start| {
        if start == 0 {
            return 0;
        }
        let mut curr = start;
        loop {
            if prefer.contains(&members[curr]) {
                break 1;
            }
            match next_of[curr] {
                Some(next) => curr = next,
                None => break 2,
            }
        }
    });

    for start in chains {
        lay_out_chain(
            start,
            &members,
            &front_of,
            &back_of,
            &next_of,
            &mut leaf,
            &mut layout,
        );
    }

    // Each chunk can be both a predecessor (back half) and a successor (front
    // half), so next_of can form cycles containing no chain start. Break every
    // remaining cycle at an arbitrary point.
    for i in 0..num_chunks {
        if !layout.contains_key(&members[i]) {
            let prev = prev_of[i]
                .take()
                .expect("chunk is neither in a chain nor a cycle");
            next_of[prev] = None;
            lay_out_chain(
                i,
                &members,
                &front_of,
                &back_of,
                &next_of,
                &mut leaf,
                &mut layout,
            );
        }
    }

    (leaf, layout)
}
