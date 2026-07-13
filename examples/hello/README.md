# hello

The smallest tour of the platform: a shared model built by `init!`, pure
handlers with their coeffects bound just above them (`home_h!`, `greet_h!`,
...), an effect-only handler (`/visit` records a row through a transactional
`Must(DbWrite)` plus an `Opt(Log)`), and the raw `Request => Response` escape
hatch (`/random`, `/proxy`, `/db`) for logic that interleaves I/O with
response building.

## Running

```sh
just run examples/hello/main.roc

# with a custom Roc checkout
ROC=/path/to/roc just run examples/hello/main.roc
```

Serves on port 8000 (`Port(8000)` in settings):

```sh
curl localhost:8000/
curl -X POST localhost:8000/greet -d '{"name":"Ada"}'
```
