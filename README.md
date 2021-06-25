[![Contributor Covenant](https://img.shields.io/badge/Contributor%20Covenant-2.0-4baaaa.svg)](CODE_OF_CONDUCT.md)

# Quickwit

This repository will host Quickwit, the big data search engine developed by Quickwit Inc.
We will progressively polish and opensource our code in the next months.

Stay tuned.


# How to use


## create the index

```
cargo run -- new --index-uri file:///path/to/indices/wikipedia-idx --doc-mapper-type wikipedia
```

## index the data

cat wiki-1000.json | RUST_LOG=info cargo run -- index --index-uri file:///path/to/indices/wikipedia-idx --heap-size 1000000000 --num-threads 1

## start the search server

cargo run -- serve --index-uris file:///path/to/indices/wikipedia-idx
```
