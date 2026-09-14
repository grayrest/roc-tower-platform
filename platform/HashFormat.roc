## Vendored from trantor-hash c1defad (components/hash/HashFormat.roc); do not edit here.
## Update by copying the file again (D-S1-8 in trantor notes/2026-09-14-s1-stdlib-roadmap-design-log.md).
import SipHash
import RapidHash

## The `Encoding` format `Hash.of` and `Hash.fast` walk a value with.
##
## The derived `encoder_for` hands every leaf of a value to this format, which
## feeds it into a byte hasher. The value's static type fixes its shape, so
## leaves carry no type tags. What is written is whatever keeps two different
## values of one type from producing the same bytes:
##
## | Value | Bytes fed |
## |---|---|
## | `U8`…`U64`, `I8`…`I64` | one little-endian `U64`, sign-extended for signed |
## | `U128`, `I128`, `Dec` | two `U64`s, low then high (`Dec` by its attos) |
## | `F32`, `F64` | the bits, after `-0.0` becomes `0.0` and every NaN the canonical quiet NaN |
## | `Bool` | `0` or `1` |
## | `Str` | its byte length, then its UTF-8 bytes |
## | null | a fixed `U64` |
## | record, tuple | fields in derive order (alphabetical for records); no names |
## | list | its length, then each item (a `Set` also arrives as a list, in insertion order) |
## | tag | the name's byte length, the name, then the payloads in order |
## | dict | its length, then the wrapping sum of per-entry hashes, each from a fresh hasher, so insertion order does not matter |
##
## The layout is pinned by `tests/layout`; changing it changes every hash and
## is a major version (D-S1-6).
HashFormat := [Default].{

	rename_field : HashFormat, Str -> Str
	rename_field = |_, name| name

	encode_str : Str, State -> Try(State, [])
	encode_str = |value, state| Ok(write_str(state, value))

	encode_bool : Bool, State -> Try(State, [])
	encode_bool = |value, state| Ok(write_u64(state, if value { 1 } else { 0 }))

	encode_u8 : U8, State -> Try(State, [])
	encode_u8 = |value, state| Ok(write_u64(state, value.to_u64()))

	encode_i8 : I8, State -> Try(State, [])
	encode_i8 = |value, state| Ok(write_u64(state, value.to_u64_wrap()))

	encode_u16 : U16, State -> Try(State, [])
	encode_u16 = |value, state| Ok(write_u64(state, value.to_u64()))

	encode_i16 : I16, State -> Try(State, [])
	encode_i16 = |value, state| Ok(write_u64(state, value.to_u64_wrap()))

	encode_u32 : U32, State -> Try(State, [])
	encode_u32 = |value, state| Ok(write_u64(state, value.to_u64()))

	encode_i32 : I32, State -> Try(State, [])
	encode_i32 = |value, state| Ok(write_u64(state, value.to_u64_wrap()))

	encode_u64 : U64, State -> Try(State, [])
	encode_u64 = |value, state| Ok(write_u64(state, value))

	encode_i64 : I64, State -> Try(State, [])
	encode_i64 = |value, state| Ok(write_u64(state, value.to_u64_wrap()))

	encode_u128 : U128, State -> Try(State, [])
	encode_u128 = |value, state| Ok(write_u128(state, value))

	encode_i128 : I128, State -> Try(State, [])
	encode_i128 = |value, state| Ok(write_u128(state, value.to_u128_wrap()))

	encode_dec : Dec, State -> Try(State, [])
	encode_dec = |value, state| Ok(write_u128(state, value.to_attos().to_u128_wrap()))

	encode_f32 : F32, State -> Try(State, [])
	encode_f32 = |value, state| Ok(write_u64(state, f32_bits(value)))

	encode_f64 : F64, State -> Try(State, [])
	encode_f64 = |value, state| Ok(write_u64(state, f64_bits(value)))

	encode_null : State -> Try(State, [])
	encode_null = |state| Ok(write_u64(state, null_marker))

	## Dict keys hash exactly as the same scalar would anywhere else.
	encode_key_str : HashFormat, Str, State -> Try(State, [])
	encode_key_str = |_, key, state| HashFormat.encode_str(key, state)

	encode_key_bool : HashFormat, Bool, State -> Try(State, [])
	encode_key_bool = |_, key, state| HashFormat.encode_bool(key, state)

	encode_key_u8 : HashFormat, U8, State -> Try(State, [])
	encode_key_u8 = |_, key, state| HashFormat.encode_u8(key, state)

	encode_key_i8 : HashFormat, I8, State -> Try(State, [])
	encode_key_i8 = |_, key, state| HashFormat.encode_i8(key, state)

	encode_key_u16 : HashFormat, U16, State -> Try(State, [])
	encode_key_u16 = |_, key, state| HashFormat.encode_u16(key, state)

	encode_key_i16 : HashFormat, I16, State -> Try(State, [])
	encode_key_i16 = |_, key, state| HashFormat.encode_i16(key, state)

	encode_key_u32 : HashFormat, U32, State -> Try(State, [])
	encode_key_u32 = |_, key, state| HashFormat.encode_u32(key, state)

	encode_key_i32 : HashFormat, I32, State -> Try(State, [])
	encode_key_i32 = |_, key, state| HashFormat.encode_i32(key, state)

	encode_key_u64 : HashFormat, U64, State -> Try(State, [])
	encode_key_u64 = |_, key, state| HashFormat.encode_u64(key, state)

	encode_key_i64 : HashFormat, I64, State -> Try(State, [])
	encode_key_i64 = |_, key, state| HashFormat.encode_i64(key, state)

	encode_key_u128 : HashFormat, U128, State -> Try(State, [])
	encode_key_u128 = |_, key, state| HashFormat.encode_u128(key, state)

	encode_key_i128 : HashFormat, I128, State -> Try(State, [])
	encode_key_i128 = |_, key, state| HashFormat.encode_i128(key, state)

	encode_key_dec : HashFormat, Dec, State -> Try(State, [])
	encode_key_dec = |_, key, state| HashFormat.encode_dec(key, state)

	encode_key_f32 : HashFormat, F32, State -> Try(State, [])
	encode_key_f32 = |_, key, state| HashFormat.encode_f32(key, state)

	encode_key_f64 : HashFormat, F64, State -> Try(State, [])
	encode_key_f64 = |_, key, state| HashFormat.encode_f64(key, state)

	encode_record : State, U64, (Container, (Container, Str, (State -> Try(State, err)) -> Try(Container, err)) -> Try(Container, err)) -> Try(State, err)
	encode_record = |state, _, write_fields| {
		done = write_fields(open(state), |box, _, write_value| item(box, write_value))?
		Ok(close(done))
	}

	encode_tuple : State, U64, (Container, (Container, (State -> Try(State, err)) -> Try(Container, err)) -> Try(Container, err)) -> Try(State, err)
	encode_tuple = |state, _, write_items| {
		done = write_items(open(state), item)?
		Ok(close(done))
	}

	encode_list : State, U64, (Container, (Container, (State -> Try(State, err)) -> Try(Container, err)) -> Try(Container, err)) -> Try(State, err)
	encode_list = |state, count, write_items| {
		done = write_items(open(write_u64(state, count)), item)?
		Ok(close(done))
	}

	encode_tag : State, Str, U64, (Container, (Container, (State -> Try(State, err)) -> Try(Container, err)) -> Try(Container, err)) -> Try(State, err)
	encode_tag = |state, name, _, write_payloads| {
		done = write_payloads(open(write_str(state, name)), item)?
		Ok(close(done))
	}

	## Each entry hashes on a fresh hasher and the results are summed, so two
	## dicts with the same entries hash the same whatever order they were built in.
	encode_dict : State, U64, (Container, (Container, (State -> Try(State, err)), (State -> Try(State, err)) -> Try(Container, err)) -> Try(Container, err)) -> Try(State, err)
	encode_dict = |state, count, write_entries| {
		done = write_entries(open(state), |box, write_key, write_value| {
			fresh = State.{ hasher: box.state.empty, empty: box.state.empty }
			keyed = write_key(fresh)?
			entry = write_value(keyed)?
			Ok(Container.{ state: box.state, sum: box.sum.plus_wrap(Bytes.finish(entry.hasher)) })
		})?
		Ok(write_u64(write_u64(done.state, count), done.sum))
	}

	## SipHash-1-3 of `value` under `key`.
	sip : a, { k0 : U64, k1 : U64 } -> U64 where [a.encoder_for : HashFormat -> (a, State -> Try(State, []))]
	sip = |value, key| hash(value, Sip(SipHash.new(key)))

	## rapidhash v3 of `value` under `seed`.
	rapid : a, U64 -> U64 where [a.encoder_for : HashFormat -> (a, State -> Try(State, []))]
	rapid = |value, seed| hash(value, Rapid(RapidHash.new(seed)))

	## The hash of `value` fed into `hasher`.
	hash : a, Bytes -> U64 where [a.encoder_for : HashFormat -> (a, State -> Try(State, []))]
	hash = |value, hasher| {
		Shape : a
		encode_value = Shape.encoder_for(HashFormat.Default)
		# Refutable, as the builtin `Json.to_str` does: the error row is empty.
		Ok(encoded) = encode_value(value, State.{ hasher, empty: hasher })
		Bytes.finish(encoded.hasher)
	}

	## Either byte hasher, so one format serves `Hash.of` and `Hash.fast`.
	Bytes := [Sip(SipHash), Rapid(RapidHash)].{

		write : Bytes, List(U8) -> Bytes
		write = |hasher, bytes|
			match hasher {
				Sip(h) => Sip(h.write(bytes))
				Rapid(h) => Rapid(h.write(bytes))
			}

		write_u64 : Bytes, U64 -> Bytes
		write_u64 = |hasher, word|
			match hasher {
				Sip(h) => Sip(h.write_u64(word))
				Rapid(h) => Rapid(h.write_u64(word))
			}

		finish : Bytes -> U64
		finish = |hasher|
			match hasher {
				Sip(h) => h.finish()
				Rapid(h) => h.finish()
			}
	}

	## The hasher being fed, and the untouched hasher a dict entry starts from.
	State := { hasher : Bytes, empty : Bytes }

	## A container in progress. `sum` accumulates a dict's entry hashes.
	Container := { state : State, sum : U64 }

	open : State -> Container
	open = |state| Container.{ state, sum: 0 }

	close : Container -> State
	close = |box| box.state

	item : Container, (State -> Try(State, err)) -> Try(Container, err)
	item = |box, write_value| {
		state = write_value(box.state)?
		Ok(Container.{ state, sum: box.sum })
	}

	write_u64 : State, U64 -> State
	write_u64 = |state, word| State.{ hasher: Bytes.write_u64(state.hasher, word), empty: state.empty }

	write_u128 : State, U128 -> State
	write_u128 = |state, value| write_u64(write_u64(state, value.to_u64_wrap()), value.shr_zf_wrap(64).to_u64_wrap())

	write_str : State, Str -> State
	write_str = |state, text| {
		bytes = Str.to_utf8(text)
		sized = write_u64(state, bytes.len())
		State.{ hasher: Bytes.write(sized.hasher, bytes), empty: state.empty }
	}

	f64_bits : F64 -> U64
	f64_bits = |value|
		if value.is_nan() {
			0x7ff8000000000000
		} else if value.to_bits() == 0x8000000000000000 {
			0
		} else {
			value.to_bits()
		}

	f32_bits : F32 -> U64
	f32_bits = |value|
		if value.is_nan() {
			0x7fc00000
		} else if value.to_bits() == 0x80000000 {
			0
		} else {
			value.to_bits().to_u64()
		}

	## "null" as ASCII, little-endian.
	null_marker : U64
	null_marker = 0x6c6c756e
}
