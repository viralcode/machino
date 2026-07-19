# machino packages

A starter ecosystem of libraries you can import like crates/npm modules.
They live in this repo so you can depend on them by **path** (today) or by
**git URL** once you pin a commit/tag.

The language prelude already covers strings, maps, JSON, and basic math —
these packages add the next layer people expect from other languages.

## Install into your project

```sh
cd myapp
machino pkg init myapp

# path dep (from a clone of this repo)
machino pkg add mathx /path/to/machino/packages/mathx
machino pkg add vec   /path/to/machino/packages/vec

# or git (when using a published tag/commit on a fork/repo)
# machino pkg add mathx https://github.com/viralcode/machino packages/mathx
# (prefer vendoring path deps or splitting packages into their own repos)

machino pkg sync
```

Then import:

```
import "pkg:mathx/mathx.mno"
import "pkg:vec/vec.mno" as vec   # optional namespace

fn main() {
    print(mathx_gcd(48, 18))
}
```

Public APIs use a package prefix (`mathx_`, `vec_`, …) so multiple packages
can be imported flat without name clashes. You can still namespace:

```
import "pkg:mathx/mathx.mno" as mx
# call mx::mathx_gcd(...)
```

## Catalog

| Package | Like… | What you get |
|---|---|---|
| [`option`](option/) | Rust `Option` | `IntMaybe` / `StrMaybe` helpers |
| [`result`](result/) | Rust `Result` | string `Ok`/`Err` helpers |
| [`vec`](vec/) | JS `Array` extras | map/filter/slice/rev/concat |
| [`mathx`](mathx/) | Python `math` extras | gcd/lcm/clamp/lerp/primes/modpow |
| [`algo`](algo/) | C++ `<algorithm>` | binary search, unique, partition |
| [`heap`](heap/) | Java `PriorityQueue` | int min-heap |
| [`queue`](queue/) / [`stack`](stack/) | collections | FIFO / LIFO |
| [`set`](set/) | Python `set` | `IntSet` / `StrSet` |
| [`encoding`](encoding/) | Python `binascii`/`base64` | hex, base64, URL encode |
| [`csv`](csv/) | Python `csv` (simple) | parse/serialize rows |
| [`httpkit`](httpkit/) | Go `net/http` helpers | parse request, build responses |
| [`cli`](cli/) | Python `argparse` lite | flags, opts, positionals |
| [`pathutil`](pathutil/) | Node `path` | join/basename/dirname/ext |
| [`urlparse`](urlparse/) | Python `urllib.parse` | scheme/host/path/query |
| [`stats`](stats/) | numpy basics | sum/min/max/mean/median |
| [`rand`](rand/) | Python `random` | seeded LCG |
| [`log`](log/) | slog / console | debug/info/warn/error |
| [`text`](text/) | string utils | pad, indent, replace, lines |
| [`jsonutil`](jsonutil/) | lodash-get for JSON | path get, as_str/as_num |
| [`graph`](graph/) | networkx lite | adjacency list + BFS |
| [`datetime`](datetime/) | datetime deltas | add days, same-day, ISO |
| [`bitset`](bitset/) | bit twiddling | set/clear/test/count |

## Try the demo

```sh
cd packages/demo
machino pkg sync
machino test main.mno
machino run main.mno
```

## Run package tests

Each library file includes `test` blocks:

```sh
for d in packages/*/ ; do
  name=$(basename "$d")
  [ "$name" = "demo" ] && continue
  ./target/release/machino test "$d${name}.mno" || exit 1
done
```

## Design notes

- **No duplicate stdlib** — don’t reimplement `json_parse`, `split`, `HashMap`, etc.
- **Contracts** on edge cases (`requires`) so `machino fuzz` / runtime checks help agents.
- **Host-free** — packages are pure machino except demos that use `extern` (`cli` takes `argv` you pass in).
- **Registry** — hosted package registry is not in-tree yet; path + git deps work today.
  Splitting a package into its own git repo is the best way to version it independently.

## Adding a new package

```sh
mkdir -p packages/foo
cat > packages/foo/machino.pkg <<EOF
name foo
version 0.1.0
EOF
# write packages/foo/foo.mno with prefixed APIs + test blocks
```

Then from an app: `machino pkg add foo ../packages/foo`.
