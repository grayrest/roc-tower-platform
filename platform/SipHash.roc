## Vendored from trantor-hash c1defad (components/hash/SipHash.roc); do not edit here.
## Update by copying the file again (D-S1-8 in trantor notes/2026-09-14-s1-stdlib-roadmap-design-log.md).
import HashVectors

## SipHash-1-3: a keyed 64-bit hash, fed incrementally.
##
## Keyed means that without the 128-bit key nobody can construct inputs that
## collide, which is what a hash over attacker-controlled input needs (a cache
## key, a table of request fields). It is the hash behind Rust's `HashMap`.
## Output for a given key and byte stream never changes; it matches the
## reference implementation, and the expects below check the Rust `siphasher`
## crate's vectors.
##
## ```roc
## hash = SipHash.new(key).write(Str.to_utf8("body")).write_u64(7).finish()
## ```
##
## Writing bytes in pieces gives the same hash as writing them at once.
SipHash :: { v0 : U64, v1 : U64, v2 : U64, v3 : U64, tail : U64, tail_len : U8, length : U64 }.{

	## A hasher with no bytes written, under `key`.
	new : { k0 : U64, k1 : U64 } -> SipHash
	new = |{ k0, k1 }| SipHash.{
		v0: k0.bitwise_xor(0x736f6d6570736575),
		v1: k1.bitwise_xor(0x646f72616e646f6d),
		v2: k0.bitwise_xor(0x6c7967656e657261),
		v3: k1.bitwise_xor(0x7465646279746573),
		tail: 0,
		tail_len: 0,
		length: 0,
	}

	## `bytes` appended to the stream.
	write : SipHash, List(U8) -> SipHash
	write = |hasher, bytes| {
		topped = top_up(hasher, bytes, 0)
		words(topped.hasher, bytes, topped.at)
	}

	## The eight little-endian bytes of `word` appended to the stream.
	write_u64 : SipHash, U64 -> SipHash
	write_u64 = |hasher, word|
		if hasher.tail_len == 0 {
			compress({ ..hasher, length: hasher.length.plus_wrap(8) }, word)
		} else {
			write(hasher, word_bytes(word))
		}

	## The hash of every byte written so far. The hasher can keep being written to.
	finish : SipHash -> U64
	finish = |hasher| {
		last = hasher.length.bitwise_and(0xff).shl_wrap(56).bitwise_or(hasher.tail)
		v = round({ v0: hasher.v0, v1: hasher.v1, v2: hasher.v2, v3: hasher.v3.bitwise_xor(last) })
		f = { v0: v.v0.bitwise_xor(last), v1: v.v1, v2: v.v2.bitwise_xor(0xff), v3: v.v3 }
		out = round(round(round(f)))
		out.v0.bitwise_xor(out.v1).bitwise_xor(out.v2).bitwise_xor(out.v3)
	}

	## Fill a partial tail word from `bytes`, compressing it once full. Returns
	## where in `bytes` the whole words start.
	top_up : SipHash, List(U8), U64 -> { hasher : SipHash, at : U64 }
	top_up = |hasher, bytes, at|
		if hasher.tail_len == 0 {
			{ hasher, at }
		} else {
			match bytes.get(at) {
				Err(_) => { hasher, at }
				Ok(byte) => {
					tail = hasher.tail.bitwise_or(U8.to_u64(byte).shl_wrap(hasher.tail_len * 8))
					grown = { ..hasher, tail, tail_len: hasher.tail_len + 1, length: hasher.length.plus_wrap(1) }
					if grown.tail_len == word_len {
						top_up(compress({ ..grown, tail: 0, tail_len: 0 }, tail), bytes, at + 1)
					} else {
						top_up(grown, bytes, at + 1)
					}
				}
			}
		}

	## Compress every whole word of `bytes` from `at`, then keep the rest as the tail.
	words : SipHash, List(U8), U64 -> SipHash
	words = |hasher, bytes, at|
		match U64.from_le_bytes(bytes, at) {
			Ok(word) => words(compress({ ..hasher, length: hasher.length.plus_wrap(8) }, word), bytes, at + 8)
			Err(_) => keep_tail(hasher, bytes, at)
		}

	keep_tail : SipHash, List(U8), U64 -> SipHash
	keep_tail = |hasher, bytes, at|
		match bytes.get(at) {
			Err(_) => hasher
			Ok(byte) =>
				keep_tail(
					{ ..hasher,
						tail: hasher.tail.bitwise_or(U8.to_u64(byte).shl_wrap(hasher.tail_len * 8)),
						tail_len: hasher.tail_len + 1,
						length: hasher.length.plus_wrap(1),
					},
					bytes,
					at + 1,
				)
		}

	## One message word through one SipRound (the "1" in SipHash-1-3).
	compress : SipHash, U64 -> SipHash
	compress = |hasher, word| {
		v = round({ v0: hasher.v0, v1: hasher.v1, v2: hasher.v2, v3: hasher.v3.bitwise_xor(word) })
		{ ..hasher, v0: v.v0.bitwise_xor(word), v1: v.v1, v2: v.v2, v3: v.v3 }
	}

	round : { v0 : U64, v1 : U64, v2 : U64, v3 : U64 } -> { v0 : U64, v1 : U64, v2 : U64, v3 : U64 }
	round = |{ v0, v1, v2, v3 }| {
		a0 = v0.plus_wrap(v1)
		a1 = rotl(v1, 13).bitwise_xor(a0)
		b0 = rotl(a0, 32)
		b2 = v2.plus_wrap(v3)
		b3 = rotl(v3, 16).bitwise_xor(b2)
		c0 = b0.plus_wrap(b3)
		c3 = rotl(b3, 21).bitwise_xor(c0)
		c2 = b2.plus_wrap(a1)
		c1 = rotl(a1, 17).bitwise_xor(c2)
		{ v0: c0, v1: c1, v2: rotl(c2, 32), v3: c3 }
	}

	rotl : U64, U8 -> U64
	rotl = |x, n| x.shl_wrap(n).bitwise_or(x.shr_zf_wrap(64 - n))

	word_bytes : U64 -> List(U8)
	word_bytes = |word| [0, 8, 16, 24, 32, 40, 48, 56].map(|shift| word.shr_zf_wrap(shift).to_u8_wrap())

	word_len : U8
	word_len = 8
}

expect
	HashVectors.sip13.all(|v| SipHash.new(HashVectors.sip_key).write(HashVectors.message(v.len)).finish() == v.hash)

expect
	# Splitting a message at any point, or into three pieces, gives the bulk hash.
	HashVectors.sip13.all(|v| {
		bytes = HashVectors.message(v.len)
		key = HashVectors.sip_key
		List.repeat(0, v.len + 1).map_with_index(|_, cut| cut).all(|cut| {
			two = SipHash.new(key).write(bytes.take_first(cut)).write(bytes.drop_first(cut)).finish()
			third = cut // 3
			three = SipHash.new(key).write(bytes.take_first(third)).write(bytes.sublist({ start: third, len: cut - third })).write(bytes.drop_first(cut)).finish()
			two == v.hash and three == v.hash
		})
	})

expect {
	bytes = HashVectors.message(40)
	word = U64.from_le_bytes(bytes, 3) ?? 0
	SipHash.new(HashVectors.sip_key).write(bytes.take_first(3)).write_u64(word).write(bytes.drop_first(11)).finish()
	== SipHash.new(HashVectors.sip_key).write(bytes).finish()
}
