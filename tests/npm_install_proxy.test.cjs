// The npm wrapper's installer downloads through the proxy HTTPS_PROXY or HTTP_PROXY names, over a
// CONNECT tunnel, and reaches the hosts NO_PROXY lists directly.
const assert = require("node:assert/strict");
const { execFile, execFileSync } = require("node:child_process");
const fs = require("node:fs");
const http = require("node:http");
const https = require("node:https");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { after, before, test } = require("node:test");

const INSTALL = path.join(__dirname, "..", "npm-package", "install.js");
const { proxyFor, proxyHint } = require(INSTALL);

const PROXY_VARIABLES = ["HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"];
const UNRESOLVABLE_HOST = "basemind.test";
const CHECKSUMS = "abc123  basemind-x86_64-unknown-linux-gnu.tar.gz\n";
const ARCHIVE = "archive bytes";

// Fetches `url` (or downloads it to `dest`) in a child process, which reads the proxy variables and
// trusts the test certificate at startup.
const RUNNER = `
const fs = require("node:fs");
const [install, url, dest] = process.argv.slice(1);
const { downloadWithRedirects, fetchTextWithRedirects } = require(install);
const run = dest
  ? downloadWithRedirects(url, dest).then(() => fs.readFileSync(dest, "utf8"))
  : fetchTextWithRedirects(url);
run.then(
  (text) => process.stdout.write(text),
  (error) => {
    process.stderr.write(error.message);
    process.exitCode = 1;
  },
);
`;

let scratch;
let certFile;
let origin;
let originPort;

function listening(server) {
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

function closed(server) {
  return new Promise((resolve) => {
    server.close(() => resolve());
    server.closeAllConnections?.();
  });
}

// A proxy that records each CONNECT and its Proxy-Authorization header, answers with `status`, and on
// 200 tunnels to the origin whatever host the request names.
async function proxyStub(status = 200) {
  const connects = [];
  const server = http.createServer((req, res) => {
    res.statusCode = 405;
    res.end();
  });
  const tunnels = new Set();
  server.on("connect", (req, clientSocket) => {
    tunnels.add(clientSocket);
    connects.push({ target: req.url, authorization: req.headers["proxy-authorization"] ?? null });
    if (status !== 200) {
      clientSocket.end(`HTTP/1.1 ${status} Refused\r\n\r\n`);
      return;
    }
    const upstream = net.connect(originPort, "127.0.0.1", () => {
      tunnels.add(upstream);
      clientSocket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      upstream.pipe(clientSocket);
      clientSocket.pipe(upstream);
    });
    upstream.on("error", () => clientSocket.destroy());
    clientSocket.on("error", () => upstream.destroy());
  });
  await listening(server);
  const close = () => {
    for (const socket of tunnels) socket.destroy();
    return closed(server);
  };
  return { close, connects, url: `http://127.0.0.1:${server.address().port}` };
}

function fetchInChild(url, env, dest) {
  const childEnv = { ...process.env, NODE_EXTRA_CA_CERTS: certFile, ...env };
  for (const name of PROXY_VARIABLES) {
    if (!(name in env)) delete childEnv[name];
    delete childEnv[name.toLowerCase()];
  }
  const args = ["-e", RUNNER, INSTALL, url, ...(dest ? [dest] : [])];
  return new Promise((resolve) => {
    execFile(process.execPath, args, { env: childEnv }, (error, stdout, stderr) => {
      resolve({ code: error ? error.code : 0, stdout, stderr });
    });
  });
}

before(async () => {
  scratch = fs.mkdtempSync(path.join(os.tmpdir(), "basemind-npm-proxy-"));
  const keyFile = path.join(scratch, "key.pem");
  certFile = path.join(scratch, "cert.pem");
  execFileSync(
    "openssl",
    [
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-days",
      "1",
      "-subj",
      "/CN=localhost",
      "-addext",
      `subjectAltName=DNS:${UNRESOLVABLE_HOST},IP:127.0.0.1`,
      "-keyout",
      keyFile,
      "-out",
      certFile,
    ],
    { stdio: "ignore" },
  );
  origin = https.createServer({ key: fs.readFileSync(keyFile), cert: fs.readFileSync(certFile) }, (req, res) => {
    if (req.url === "/redirect") {
      res.writeHead(302, { Location: "/archive" });
      res.end();
      return;
    }
    res.end(req.url === "/archive" ? ARCHIVE : CHECKSUMS);
  });
  await listening(origin);
  originPort = origin.address().port;
});

after(async () => {
  await closed(origin);
  fs.rmSync(scratch, { recursive: true, force: true });
});

test("HTTPS_PROXY carries the download through a CONNECT tunnel", async () => {
  const proxy = await proxyStub();
  try {
    const result = await fetchInChild(`https://${UNRESOLVABLE_HOST}:${originPort}/checksums.txt`, {
      HTTPS_PROXY: proxy.url,
    });
    assert.deepEqual(result, { code: 0, stdout: CHECKSUMS, stderr: "" });
    assert.deepEqual(proxy.connects, [{ target: `${UNRESOLVABLE_HOST}:${originPort}`, authorization: null }]);
  } finally {
    await proxy.close();
  }
});

test("a redirect's target goes through the proxy too", async () => {
  const proxy = await proxyStub();
  const dest = path.join(scratch, "redirected.tar.gz");
  try {
    const result = await fetchInChild(
      `https://${UNRESOLVABLE_HOST}:${originPort}/redirect`,
      { HTTP_PROXY: proxy.url, NO_PROXY: "example.com" },
      dest,
    );
    assert.deepEqual(result, { code: 0, stdout: ARCHIVE, stderr: "" });
    assert.deepEqual(
      proxy.connects.map(({ target }) => target),
      [`${UNRESOLVABLE_HOST}:${originPort}`, `${UNRESOLVABLE_HOST}:${originPort}`],
    );
  } finally {
    await proxy.close();
  }
});

test("proxy credentials in the URL are sent as Proxy-Authorization", async () => {
  const proxy = await proxyStub();
  const withCredentials = proxy.url.replace("http://", "http://user%40corp:p%3Ass@");
  try {
    const result = await fetchInChild(`https://${UNRESOLVABLE_HOST}:${originPort}/checksums.txt`, {
      HTTPS_PROXY: withCredentials,
    });
    assert.equal(result.code, 0);
    assert.deepEqual(proxy.connects, [
      {
        target: `${UNRESOLVABLE_HOST}:${originPort}`,
        authorization: `Basic ${Buffer.from("user@corp:p:ss").toString("base64")}`,
      },
    ]);
  } finally {
    await proxy.close();
  }
});

test("a host NO_PROXY lists is fetched directly", async () => {
  for (const noProxy of ["127.0.0.1", "*", `localhost, 127.0.0.1:${originPort}`]) {
    const proxy = await proxyStub();
    try {
      const result = await fetchInChild(`https://127.0.0.1:${originPort}/checksums.txt`, {
        HTTPS_PROXY: proxy.url,
        NO_PROXY: noProxy,
      });
      assert.deepEqual(result, { code: 0, stdout: CHECKSUMS, stderr: "" }, noProxy);
      assert.deepEqual(proxy.connects, [], noProxy);
    } finally {
      await proxy.close();
    }
  }
});

test("a refused CONNECT fails with the proxy's status", async () => {
  const proxy = await proxyStub(407);
  try {
    const result = await fetchInChild(`https://${UNRESOLVABLE_HOST}:${originPort}/checksums.txt`, {
      HTTPS_PROXY: proxy.url,
    });
    const address = proxy.url.replace("http://", "");
    assert.deepEqual(result, {
      code: 1,
      stdout: "",
      stderr: `proxy ${address} refused CONNECT ${UNRESOLVABLE_HOST}:${originPort}: HTTP 407`,
    });
  } finally {
    await proxy.close();
  }
});

const PROXY_CASES = [
  { name: "no proxy variable", env: {}, url: "https://github.com/", proxy: null },
  { name: "HTTPS_PROXY", env: { HTTPS_PROXY: "http://proxy:3128" }, url: "https://github.com/", proxy: "proxy:3128" },
  {
    name: "HTTP_PROXY fallback",
    env: { HTTP_PROXY: "http://web:8080" },
    url: "https://github.com/",
    proxy: "web:8080",
  },
  {
    name: "HTTPS_PROXY over HTTP_PROXY",
    env: { HTTPS_PROXY: "http://secure:1", HTTP_PROXY: "http://plain:2" },
    url: "https://github.com/",
    proxy: "secure:1",
  },
  {
    name: "lowercase over uppercase",
    env: { https_proxy: "http://lower:1", HTTPS_PROXY: "http://upper:2" },
    url: "https://github.com/",
    proxy: "lower:1",
  },
  { name: "scheme-less value", env: { HTTPS_PROXY: "proxy:3128" }, url: "https://github.com/", proxy: "proxy:3128" },
  { name: "NO_PROXY *", env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "*" }, url: "https://github.com/", proxy: null },
  {
    name: "NO_PROXY exact host",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "github.com" },
    url: "https://github.com/",
    proxy: null,
  },
  {
    name: "NO_PROXY bare host covers subdomains",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "githubusercontent.com" },
    url: "https://objects.githubusercontent.com/",
    proxy: null,
  },
  {
    name: "NO_PROXY leading dot",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: ".githubusercontent.com" },
    url: "https://objects.githubusercontent.com/",
    proxy: null,
  },
  {
    name: "NO_PROXY leading *.",
    env: { HTTPS_PROXY: "proxy:1", no_proxy: "*.githubusercontent.com" },
    url: "https://objects.githubusercontent.com/",
    proxy: null,
  },
  {
    name: "NO_PROXY suffix only at a label boundary",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "hub.com" },
    url: "https://github.com/",
    proxy: "proxy:1",
  },
  {
    name: "NO_PROXY is case-insensitive and space-separated",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "example.org  GitHub.COM" },
    url: "https://github.com/",
    proxy: null,
  },
  {
    name: "NO_PROXY port matches the default port",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "github.com:443" },
    url: "https://github.com/",
    proxy: null,
  },
  {
    name: "NO_PROXY port limits the entry",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "github.com:8443" },
    url: "https://github.com/",
    proxy: "proxy:1",
  },
  {
    name: "NO_PROXY IPv6",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "[::1]" },
    url: "https://[::1]:8443/",
    proxy: null,
  },
  {
    name: "NO_PROXY for another host",
    env: { HTTPS_PROXY: "proxy:1", NO_PROXY: "localhost,127.0.0.1" },
    url: "https://github.com/",
    proxy: "proxy:1",
  },
];

for (const { name, env, url, proxy } of PROXY_CASES) {
  test(`proxyFor: ${name}`, () => {
    const chosen = proxyFor(new URL(url), env);
    assert.equal(chosen && chosen.host, proxy);
  });
}

test("a proxy that is not http:// is refused naming its variable", () => {
  assert.throws(() => proxyFor(new URL("https://github.com/"), { HTTPS_PROXY: "socks5://proxy:1080" }), {
    message: "HTTPS_PROXY uses socks5: — only http:// proxies are supported",
  });
});

test("the failure hint names the proxy variables and hides credentials", () => {
  assert.equal(
    proxyHint({}),
    "No proxy is set. Behind a proxy, set HTTPS_PROXY (or HTTP_PROXY) to its http:// URL and NO_PROXY to the hosts to reach directly.",
  );
  assert.equal(
    proxyHint({ HTTP_PROXY: "http://user:secret@proxy:3128", NO_PROXY: "localhost" }),
    "Proxy: HTTP_PROXY=http://proxy:3128/, NO_PROXY=localhost.",
  );
  assert.equal(proxyHint({ https_proxy: "proxy:3128" }), "Proxy: HTTPS_PROXY=http://proxy:3128/, NO_PROXY=(unset).");
});
