// The npm wrapper's installer downloads over HTTPS only, and a refused URL makes no request.
const assert = require("node:assert/strict");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");

const { downloadWithRedirects, fetchTextWithRedirects } = require("../npm-package/install.js");

function listen(handler) {
  return new Promise((resolve) => {
    const server = http.createServer(handler);
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

test("a plain-HTTP URL is refused before any request is made", async () => {
  let requests = 0;
  const server = await listen((req, res) => {
    requests += 1;
    res.end("served");
  });
  const url = `http://127.0.0.1:${server.address().port}/basemind.tar.gz`;
  try {
    const dest = path.join(os.tmpdir(), `basemind-npm-test-${process.pid}`);
    await assert.rejects(downloadWithRedirects(url, dest), /only https: is allowed/u);
    await assert.rejects(fetchTextWithRedirects(url), /only https: is allowed/u);
    assert.equal(requests, 0);
  } finally {
    server.close();
  }
});

test("an exhausted redirect budget rejects without a request", async () => {
  await assert.rejects(fetchTextWithRedirects("https://127.0.0.1:9/checksums.txt", 0), /Too many redirects/u);
});
