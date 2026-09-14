## Vendored from trantor-hash c1defad (components/hash/Hash.roc); do not edit here.
## Update by copying the file again (D-S1-8 in trantor notes/2026-09-14-s1-stdlib-roadmap-design-log.md).
import HashFormat
import SipHash
import RapidHash

## Structural hashes of any value that can be encoded.
##
## Two hashes, chosen by what the hash is for:
##
## - `Hash.of` is SipHash-1-3 under a 128-bit key. Use it when inputs can come
##   from someone who wants two of them to collide: a cache key over request
##   bodies, a table of client-supplied fields. Keep the key secret and random.
## - `Hash.fast` is rapidhash v3 under a `U64` seed. Use it for ids and tables
##   whose inputs are your own. It is not attack-resistant.
##
## Both walk the value through its derived encoder, so records, tag unions,
## lists, tuples and dicts all hash. Equal values hash equal; a `Dict` hashes
## the same whatever order it was built in. A `Set` arrives as a list, so its
## hash follows insertion order. The bytes each kind of value contributes are
## listed in `HashFormat`.
##
## Hashes are stable within a major version of trantor-hash: the algorithms
## match their reference vectors and the structural layout is pinned by tests.
##
## ```roc
## key = Hash.of(decoded_body, secret_key)
## id = Hash.fast(slug, 0)
## ```
Hash :: [].{

	## A SipHash key: two random `U64`s.
	Key : { k0 : U64, k1 : U64 }

	## SipHash-1-3 of `value` under `key`.
	of : a, Key -> U64 where [a.encoder_for : HashFormat -> (a, HashFormat.State -> Try(HashFormat.State, []))]
	of = |value, key| HashFormat.sip(value, key)

	## rapidhash v3 of `value` under `seed`.
	fast : a, U64 -> U64 where [a.encoder_for : HashFormat -> (a, HashFormat.State -> Try(HashFormat.State, []))]
	fast = |value, seed| HashFormat.rapid(value, seed)
}

key : Hash.Key
key = { k0: 0x0123456789abcdef, k1: 0xfedcba9876543210 }

expect Hash.of("abc", key) == Hash.of("abc", key)
expect Hash.of("abc", key) != Hash.of("abd", key)
expect Hash.of("abc", key) != Hash.of("abc", { k0: key.k0, k1: key.k1 + 1 })
expect Hash.fast("abc", 0) != Hash.fast("abc", 1)

# Adjacent strings and lists cannot trade bytes or items.
expect Hash.of(["ab", "c"], key) != Hash.of(["a", "bc"], key)
expect Hash.fast(["ab", "c"], 0) != Hash.fast(["a", "bc"], 0)
expect Hash.of(("ab", "c"), key) != Hash.of(("a", "bc"), key)
expect {
	a : List(List(U64))
	a = [[1], []]
	b : List(List(U64))
	b = [[], [1]]
	Hash.of(a, key) != Hash.of(b, key)
}

# Tags are told apart by name even when their payloads agree.
expect {
	a : [Good(U64), Bad(U64)]
	a = Good(0)
	b : [Good(U64), Bad(U64)]
	b = Bad(0)
	Hash.of(a, key) != Hash.of(b, key)
}

# Records hash by value, whatever order the fields were written in.
expect Hash.of({ id: 1.I64, title: "t" }, key) == Hash.of({ title: "t", id: 1.I64 }, key)
expect Hash.of({ id: 1.I64, title: "t" }, key) != Hash.of({ id: 2.I64, title: "t" }, key)

# A dict hashes the same whatever order it was built in, and not the same as a different dict.
expect {
	a = Dict.from_list([("x", 1.U64), ("y", 2)])
	b = Dict.from_list([("y", 2.U64), ("x", 1)])
	c = Dict.from_list([("x", 2.U64), ("y", 1)])
	Hash.of(a, key) == Hash.of(b, key) and Hash.of(a, key) != Hash.of(c, key) and Hash.fast(a, 0) == Hash.fast(b, 0)
}

# -0.0 equals 0.0, and every NaN is one NaN.
expect Hash.of(-0.0.F64, key) == Hash.of(0.0.F64, key)
expect Hash.of(F64.nan, key) == Hash.of(F64.nan.negate(), key)
expect Hash.of(-0.0.F32, key) == Hash.of(0.0.F32, key)

# Signed integers are sign-extended, so -1 is not 255.
expect Hash.of(-1.I8, key) != Hash.of(255.U8, key)

# The structural hash of a string is the byte hash of its length then its bytes.
expect {
	text = "hello"
	bytes = Str.to_utf8(text)
	Hash.fast(text, 7) == RapidHash.new(7).write_u64(bytes.len()).write(bytes).finish()
	and Hash.of(text, key) == SipHash.new(key).write_u64(bytes.len()).write(bytes).finish()
}
