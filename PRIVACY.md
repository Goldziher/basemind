# Privacy Policy

## Last updated: 2026-06-17

basemind is a local-first developer tool. It runs entirely on your own machine
as a command-line scanner and a stdio MCP server. **basemind collects no
personal data, contains no analytics or telemetry that leaves your machine, and
never transmits your code, queries, or any other information to its authors or
any third party.**

## What basemind stores, and where

- **Index data** — When you scan a repository, basemind writes a content-addressed
  index to a `.basemind/` directory inside that repository (extraction blobs plus
  a local inverted index). This never leaves your disk.
- **Local usage counters** — basemind records per-tool call counts and estimated
  token savings to `.basemind/telemetry.jsonl` so the optional status line can show
  activity. This file is written to your local disk only. It is **never uploaded,
  transmitted, or shared**, and contains no source code — only tool names, a
  non-reversible hash of parameters, response sizes, and timings.

## When basemind makes network requests

basemind only contacts the network when **you** explicitly direct it to:

- **Web crawl tools** (`web_scrape`, `web_crawl`, `web_map`) fetch the URLs you
  ask them to fetch. Available only when the optional `crawl` feature is enabled.
- **LLM-backed document analysis** (summarization, NER) sends content to the LLM
  provider **you configure** via your own API key. Off unless you enable it and
  supply a key. Your key is held in memory for the request and masked in all logs.
- **Installation** — The npm and PyPI wrapper packages download the basemind
  binary from this project's GitHub Releases during install. This is a standard
  package-download request to GitHub.

basemind has no built-in calls to any service operated by its authors. There is
no account, no sign-in, and no server operated by basemind.

## Data sent to LLM providers

If you enable LLM-backed features, the data you process is sent to the provider
you chose, under that provider's privacy policy and your agreement with them.
basemind does not see, store, or proxy that traffic.

## Children's privacy

basemind is a developer tool and is not directed at children.

## Changes

Material changes to this policy will be reflected in this file with an updated
date.

## Contact

Questions: open an issue at <https://github.com/Goldziher/basemind/issues> or
email the maintainer at <nhirschfeld@gmail.com>.
