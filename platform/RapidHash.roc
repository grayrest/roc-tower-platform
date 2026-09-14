## Vendored from trantor-hash c1defad (components/hash/RapidHash.roc); do not edit here.
## Update by copying the file again (D-S1-8 in trantor notes/2026-09-14-s1-stdlib-roadmap-design-log.md).
import HashVectors

## rapidhash v3: a fast, well-distributed 64-bit hash, fed incrementally.
##
## For ids, tables and anything that is not attacker-facing: rapidhash is not
## keyed, and seed-independent collisions are published for its family. Use
## `SipHash` when inputs come from someone who might want two of them to
## collide.
##
## Output for a given seed and byte stream never changes. It is the C++
## reference's `rapidhash_withSeed`, and the expects below check the Rust
## `rapidhash` crate's v3 vectors. Writing bytes in pieces gives the same hash
## as writing them at once.
##
## ```roc
## id = RapidHash.new(0).write(Str.to_utf8(slug)).finish()
## ```
RapidHash :: { seed : U64, lanes : List(U64), processed : Bool, prev : List(U8), buf : List(U8) }.{

	## A hasher with no bytes written, under `seed`.
	new : U64 -> RapidHash
	new = |seed| {
		mixed = seed.bitwise_xor(mix(seed.bitwise_xor(secret(2)), secret(1)))
		RapidHash.{ seed: mixed, lanes: List.repeat(mixed, lane_count), processed: False, prev: [], buf: [] }
	}

	## `bytes` appended to the stream.
	write : RapidHash, List(U8) -> RapidHash
	write = |hasher, bytes| drain({ ..hasher, buf: hasher.buf.concat(bytes) }, 0)

	## The eight little-endian bytes of `word` appended to the stream.
	write_u64 : RapidHash, U64 -> RapidHash
	write_u64 = |hasher, word|
		write(hasher, [0, 8, 16, 24, 32, 40, 48, 56].map(|shift| word.shr_zf_wrap(shift).to_u8_wrap()))

	## The hash of every byte written so far. The hasher can keep being written to.
	finish : RapidHash -> U64
	finish = |hasher| {
		len = hasher.buf.len()
		if !hasher.processed and len <= 16 {
			short(hasher.seed, hasher.buf)
		} else {
			start = if hasher.processed { hasher.lanes.fold(0, |acc, lane| acc.bitwise_xor(lane)) } else { hasher.seed }
			seed = tail_mix(start, hasher.buf)
			data = hasher.prev.concat(hasher.buf)
			a = read_u64(data, data.len() - 16).bitwise_xor(len)
			b = read_u64(data, data.len() - 8)
			finish_mix(a, b, seed, len)
		}
	}

	## Inputs of at most 16 bytes, before any chunk was processed.
	short : U64, List(U8) -> U64
	short = |seed, data| {
		len = data.len()
		if len >= 8 {
			finish_mix(read_u64(data, 0), read_u64(data, len - 8), seed.bitwise_xor(len), len)
		} else if len >= 4 {
			finish_mix(read_u32(data, 0), read_u32(data, len - 4), seed.bitwise_xor(len), len)
		} else if len > 0 {
			first = U8.to_u64(data.get(0) ?? 0)
			last = U8.to_u64(data.get(len - 1) ?? 0)
			a = first.shl_wrap(45).bitwise_or(last)
			b = U8.to_u64(data.get(len // 2) ?? 0)
			finish_mix(a, b, seed, len)
		} else {
			finish_mix(0, 0, seed, 0)
		}
	}

	finish_mix : U64, U64, U64, U64 -> U64
	finish_mix = |a, b, seed, remainder| {
		m = mum(a.bitwise_xor(secret(1)), b.bitwise_xor(seed))
		mix(m.low.bitwise_xor(0xaaaaaaaaaaaaaaaa), m.high.bitwise_xor(secret(1)).bitwise_xor(remainder))
	}

	## The up-to-six 16-byte mixes over the unprocessed bytes (at most 112).
	tail_mix : U64, List(U8) -> U64
	tail_mix = |seed, slice| {
		steps = [2, 2, 1, 1, 2, 1]
		steps.map_with_index(|s, i| { secret_index: s, offset: i * 16 }).fold(seed, |acc, step|
			if slice.len() > step.offset + 16 {
				mix(read_u64(slice, step.offset).bitwise_xor(secret(step.secret_index)), read_u64(slice, step.offset + 8).bitwise_xor(acc))
			} else {
				acc
			})
	}

	## Process whole 112-byte chunks while more than one chunk's worth is
	## buffered: rapidhash keeps the final chunk for `finish`, even when it is full.
	drain : RapidHash, U64 -> RapidHash
	drain = |hasher, at|
		if hasher.buf.len() - at > chunk_size {
			chunk = hasher.buf.sublist({ start: at, len: chunk_size })
			lanes = hasher.lanes.map_with_index(|lane, i|
				mix(read_u64(chunk, i * 16).bitwise_xor(secret(i)), read_u64(chunk, i * 16 + 8).bitwise_xor(lane)))
			drain({ ..hasher, lanes, processed: True, prev: chunk.drop_first(chunk_size - 16) }, at + chunk_size)
		} else {
			{ ..hasher, buf: hasher.buf.drop_first(at) }
		}

	mum : U64, U64 -> { low : U64, high : U64 }
	mum = |a, b| {
		r = a.to_u128().times_wrap(b.to_u128())
		{ low: r.to_u64_wrap(), high: r.shr_zf_wrap(64).to_u64_wrap() }
	}

	mix : U64, U64 -> U64
	mix = |a, b| {
		m = mum(a, b)
		m.low.bitwise_xor(m.high)
	}

	read_u64 : List(U8), U64 -> U64
	read_u64 = |bytes, at| U64.from_le_bytes(bytes, at) ?? 0

	read_u32 : List(U8), U64 -> U64
	read_u32 = |bytes, at| U32.to_u64(U32.from_le_bytes(bytes, at) ?? 0)

	## rapidhash's default secrets, which the C++ reference seeds with.
	secret : U64 -> U64
	secret = |i| secrets.get(i) ?? 0

	secrets : List(U64)
	secrets = [
		0x2d358dccaa6c78a5,
		0x8bb84b93962eacc9,
		0x4b33a62ed433d4a3,
		0x4d5a2da51de1aa47,
		0xa0761d6478bd642f,
		0xe7037ed1a0b428db,
		0x90ed1765281c388c,
	]

	lane_count : U64
	lane_count = 7

	chunk_size : U64
	chunk_size = 112
}

expect
	HashVectors.rapid.all(|v| RapidHash.new(v.seed).write(HashVectors.message(v.len)).finish() == v.hash)

expect
	# Splitting a message at any point, or into three pieces, gives the bulk hash.
	HashVectors.rapid.all(|v| {
		bytes = HashVectors.message(v.len)
		List.repeat(0, v.len + 1).map_with_index(|_, cut| cut).all(|cut| {
			two = RapidHash.new(v.seed).write(bytes.take_first(cut)).write(bytes.drop_first(cut)).finish()
			third = cut // 3
			three = RapidHash.new(v.seed).write(bytes.take_first(third)).write(bytes.sublist({ start: third, len: cut - third })).write(bytes.drop_first(cut)).finish()
			two == v.hash and three == v.hash
		})
	})

expect {
	bytes = HashVectors.message(40)
	word = U64.from_le_bytes(bytes, 3) ?? 0
	RapidHash.new(9).write(bytes.take_first(3)).write_u64(word).write(bytes.drop_first(11)).finish()
	== RapidHash.new(9).write(bytes).finish()
}
