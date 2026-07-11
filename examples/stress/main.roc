app [handle!] { pf: platform "../../platform/main.roc" }

# Allocation-heavy handler: builds ~100 interpolated strings and grows a
# large concatenated buffer, stressing the allocator and refcounts under
# concurrent invocation.
handle! : Str => Str
handle! = |path| {
    seeds = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    pieces = seeds.map(|i| "segment-${i.to_str()}-${path}")
    joined = Str.join_with(pieces, "/")
    doubled = seeds.fold(joined, |acc, _| Str.concat(acc, joined))
    "bytes=${Str.count_utf8_bytes(doubled).to_str()} path=${path}"
}
