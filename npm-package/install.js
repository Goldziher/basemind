const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const http = require("node:http");
const https = require("node:https");
const tls = require("node:tls");
const crypto = require("node:crypto");
const { execFileSync } = require("node:child_process");

const { version } = require("./package.json");

const REQUEST_TIMEOUT_MS = 30000;
const USER_AGENT = "basemind-npm-wrapper";
const DEFAULT_PORTS = { "http:": 80, "https:": 443 };

function isAppleSilicon() {
  if (os.type() !== "Darwin") return false;
  if (os.arch() === "arm64") return true;
  const sysctl = (name) => {
    try {
      return execFileSync("sysctl", ["-n", name], { encoding: "utf8" }).trim();
    } catch {
      return "";
    }
  };
  return sysctl("sysctl.proc_translated") === "1" || sysctl("hw.optional.arm64") === "1";
}

function getPlatformTriple(type = os.type(), arch = os.arch()) {
  if (type === "Windows_NT") {
    if (arch === "x64") return "x86_64-pc-windows-msvc";
    if (arch === "ia32") throw new Error("32-bit Windows is not supported");
  }

  if (type === "Linux") {
    if (arch === "x64") return "x86_64-unknown-linux-gnu";
    if (arch === "arm64") return "aarch64-unknown-linux-gnu";
    throw new Error(`Linux on ${arch} is not supported; basemind publishes x86_64 and aarch64 Linux binaries`);
  }

  if (type === "Darwin") {
    if (isAppleSilicon()) return "aarch64-apple-darwin";
    throw new Error("macOS on Intel (x86_64) is not supported; basemind publishes Apple Silicon (arm64) binaries only");
  }

  throw new Error(`Unsupported platform: ${type} ${arch}`);
}

function getReleaseAssets() {
  const platform = getPlatformTriple();
  const baseUrl = `https://github.com/Goldziher/basemind/releases/download/v${version}`;
  const ext = platform.includes("windows") ? "zip" : "tar.gz";
  const assetName = `basemind-${platform}.${ext}`;
  return {
    assetName,
    archiveUrl: `${baseUrl}/${assetName}`,
    checksumsUrl: `${baseUrl}/basemind_${version}_checksums.txt`,
  };
}

// Every request, a redirect's target included, goes over HTTPS: over plain HTTP anyone on the path
// could replace the archive and the checksums file together.
function httpsUrl(location, base) {
  const url = new URL(location, base);
  if (url.protocol !== "https:") {
    throw new Error(`refusing to download over ${url.protocol} (${url.href}); only https: is allowed`);
  }
  return url;
}

// The URL to request next, or a rejection that stops the chain before any request is made.
function nextUrl(url, maxRedirects, reject) {
  if (maxRedirects <= 0) {
    reject(new Error("Too many redirects"));
    return null;
  }
  try {
    return httpsUrl(url);
  } catch (error) {
    reject(error);
    return null;
  }
}

function envValue(env, name) {
  return (env[name.toLowerCase()] || env[name] || "").trim();
}

// The variable naming the proxy for an https: request, read as undici's EnvHttpProxyAgent reads it:
// the lowercase name before the uppercase one, and HTTP_PROXY when HTTPS_PROXY is unset.
function proxySetting(env) {
  for (const name of ["HTTPS_PROXY", "HTTP_PROXY"]) {
    const value = envValue(env, name);
    if (value) return { name, value };
  }
  return null;
}

// A proxy value without a scheme (`proxy.internal:3128`) is an http:// proxy, as curl reads it.
function proxyUrl(value) {
  return new URL(value.includes("://") ? value : `http://${value}`);
}

function parseProxy({ name, value }) {
  let url;
  try {
    url = proxyUrl(value);
  } catch (error) {
    throw new Error(`${name} is not a valid proxy URL`, { cause: error });
  }
  if (url.protocol !== "http:") {
    throw new Error(`${name} uses ${url.protocol} — only http:// proxies are supported`);
  }
  return url;
}

function unbracketed(host) {
  return host.replace(/^\[(.*)\]$/u, "$1");
}

function splitHostPort(entry) {
  const bracketed = entry.match(/^\[([^\]]+)\](?::(\d+))?$/u);
  if (bracketed) return { host: bracketed[1], port: bracketed[2] ? Number(bracketed[2]) : null };
  const parts = entry.split(":");
  if (parts.length === 2 && /^\d+$/u.test(parts[1])) return { host: parts[0], port: Number(parts[1]) };
  return { host: entry, port: null };
}

// A NO_PROXY entry is `*`, or a host that also covers its subdomains, written bare, with a leading
// `.` or with a leading `*.`, and optionally with a `:port` it is limited to.
function noProxyEntryMatches(entry, host, port) {
  if (entry === "*") return true;
  const target = splitHostPort(entry);
  if (target.port !== null && target.port !== port) return false;
  const suffix = target.host.replace(/^\*?\./u, "");
  return suffix !== "" && (host === suffix || host.endsWith(`.${suffix}`));
}

function bypassesProxy(url, env) {
  const host = unbracketed(url.hostname).toLowerCase();
  const port = Number(url.port) || DEFAULT_PORTS[url.protocol];
  return envValue(env, "NO_PROXY")
    .toLowerCase()
    .split(/[\s,]+/u)
    .filter(Boolean)
    .some((entry) => noProxyEntryMatches(entry, host, port));
}

// The proxy a request to `url` goes through, or null when no proxy is set or NO_PROXY names the host.
function proxyFor(url, env = process.env) {
  const setting = proxySetting(env);
  if (!setting || bypassesProxy(url, env)) return null;
  return parseProxy(setting);
}

function proxyAddress(proxy) {
  return `${proxy.hostname}:${Number(proxy.port) || DEFAULT_PORTS[proxy.protocol]}`;
}

function proxyAuthorization(proxy) {
  if (!proxy.username && !proxy.password) return {};
  const credentials = `${decodeURIComponent(proxy.username)}:${decodeURIComponent(proxy.password)}`;
  return { "Proxy-Authorization": `Basic ${Buffer.from(credentials).toString("base64")}` };
}

// A socket to `host:port` through the proxy's CONNECT tunnel; TLS to the origin runs inside it, so
// the proxy sees neither the archive nor the checksums in the clear.
function openTunnel(proxy, host, port) {
  const authority = `${host.includes(":") ? `[${host}]` : host}:${port}`;
  return new Promise((resolve, reject) => {
    const req = http.request({
      host: unbracketed(proxy.hostname),
      port: Number(proxy.port) || DEFAULT_PORTS[proxy.protocol],
      method: "CONNECT",
      path: authority,
      agent: false,
      headers: { Host: authority, "User-Agent": USER_AGENT, ...proxyAuthorization(proxy) },
    });
    req.once("connect", (res, socket) => {
      if (res.statusCode === 200) {
        resolve(socket);
        return;
      }
      socket.destroy();
      reject(new Error(`proxy ${proxyAddress(proxy)} refused CONNECT ${authority}: HTTP ${res.statusCode}`));
    });
    req.once("error", (error) => {
      reject(new Error(`proxy ${proxyAddress(proxy)} failed: ${error.message}`, { cause: error }));
    });
    req.setTimeout(REQUEST_TIMEOUT_MS, () => {
      req.destroy(new Error("Download timeout"));
    });
    req.end();
  });
}

function tunnelAgent(proxy) {
  const agent = new https.Agent();
  agent.createConnection = (options, callback) => {
    openTunnel(proxy, options.host, options.port).then(
      (socket) => callback(null, tls.connect({ ...options, socket })),
      callback,
    );
  };
  return agent;
}

function requestOptions(url) {
  const headers = { "User-Agent": USER_AGENT };
  const proxy = proxyFor(url);
  return proxy ? { headers, agent: tunnelAgent(proxy) } : { headers };
}

function redactedProxy(value) {
  try {
    const url = proxyUrl(value);
    url.username = "";
    url.password = "";
    return url.href;
  } catch {
    return "(not a valid URL)";
  }
}

// The proxy settings line printed with an install failure, credentials removed.
function proxyHint(env = process.env) {
  const setting = proxySetting(env);
  if (!setting) {
    return "No proxy is set. Behind a proxy, set HTTPS_PROXY (or HTTP_PROXY) to its http:// URL and NO_PROXY to the hosts to reach directly.";
  }
  const noProxy = envValue(env, "NO_PROXY") || "(unset)";
  return `Proxy: ${setting.name}=${redactedProxy(setting.value)}, NO_PROXY=${noProxy}.`;
}

function downloadWithRedirects(url, dest, maxRedirects = 5) {
  return new Promise((resolve, reject) => {
    const urlObj = nextUrl(url, maxRedirects, reject);
    if (!urlObj) return;

    const req = https.get(urlObj, requestOptions(urlObj), (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        downloadWithRedirects(new URL(res.headers.location, urlObj).href, dest, maxRedirects - 1)
          .then(resolve)
          .catch(reject);
        return;
      }

      if (res.statusCode !== 200) {
        reject(new Error(`HTTP ${res.statusCode}: ${res.statusMessage}`));
        return;
      }

      const file = fs.createWriteStream(dest);
      res.pipe(file);

      file.on("finish", () => {
        file.close();
        resolve();
      });

      file.on("error", (err) => {
        fs.unlink(dest, () => {});
        reject(err);
      });
    });

    req.on("error", reject);
    req.setTimeout(REQUEST_TIMEOUT_MS, () => {
      req.destroy();
      reject(new Error("Download timeout"));
    });
  });
}

function retryWithBackoff(fn, maxAttempts = 3) {
  const delays = [1000, 2000, 4000];
  return (async function attempt(index = 0) {
    try {
      return await fn();
    } catch (err) {
      const statusMatch = err.message.match(/HTTP ([0-9]+)/u);
      const isRetryable =
        err.message.includes("Download timeout") ||
        err.message.includes("ECONNREFUSED") ||
        err.message.includes("ECONNRESET") ||
        err.message.includes("ETIMEDOUT") ||
        err.message.includes("EHOSTUNREACH") ||
        (statusMatch && Math.trunc(Number(statusMatch[1])) >= 500);

      if (!isRetryable || index >= maxAttempts - 1) {
        throw err;
      }

      const delay = delays[index];
      console.log(`Transient error (attempt ${index + 1}/${maxAttempts}): ${err.message}; retrying in ${delay}ms...`);
      await new Promise((resolve) => {
        setTimeout(resolve, delay);
      });
      return attempt(index + 1);
    }
  })();
}

function fetchTextWithRedirects(url, maxRedirects = 5) {
  return new Promise((resolve, reject) => {
    const urlObj = nextUrl(url, maxRedirects, reject);
    if (!urlObj) return;

    const req = https.get(urlObj, requestOptions(urlObj), (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        fetchTextWithRedirects(new URL(res.headers.location, urlObj).href, maxRedirects - 1)
          .then(resolve)
          .catch(reject);
        return;
      }

      if (res.statusCode !== 200) {
        reject(new Error(`HTTP ${res.statusCode}: ${res.statusMessage}`));
        return;
      }

      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
      res.on("error", reject);
    });

    req.on("error", reject);
    req.setTimeout(REQUEST_TIMEOUT_MS, () => {
      req.destroy();
      reject(new Error("Download timeout"));
    });
  });
}

function retryFetchText(url) {
  return retryWithBackoff(() => fetchTextWithRedirects(url));
}

function sha256File(filePath) {
  const hash = crypto.createHash("sha256");
  hash.update(fs.readFileSync(filePath));
  return hash.digest("hex");
}

function expectedDigest(checksumsText, assetName) {
  for (const line of checksumsText.split(/\r?\n/u)) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const parts = trimmed.split(/\s+/u);
    if (parts.length < 2) continue;
    const digest = parts[0];
    const name = parts.at(-1).replace(/^\*/u, "");
    if (name === assetName) return digest.toLowerCase();
  }
  return null;
}

async function verifyChecksum(archivePath, assetName, checksumsUrl) {
  let checksumsText;
  try {
    checksumsText = await retryFetchText(checksumsUrl);
  } catch (error) {
    throw new Error(
      `could not fetch checksums (${checksumsUrl}): ${error.message} — refusing to install unverified binary`,
      { cause: error },
    );
  }

  const expected = expectedDigest(checksumsText, assetName);
  if (!expected) {
    throw new Error(`no checksum entry for ${assetName} in ${checksumsUrl} — refusing to install unverified binary`);
  }

  const actual = sha256File(archivePath).toLowerCase();
  if (actual !== expected) {
    throw new Error(`checksum mismatch for ${assetName} (expected ${expected}, got ${actual})`);
  }

  console.log("Checksum verified.");
}

async function extractArchive(archivePath, destination) {
  if (archivePath.endsWith(".zip")) {
    const AdmZip = require("adm-zip");
    new AdmZip(archivePath).extractAllTo(destination, true);
  } else {
    const { extract } = await import("tar");
    await extract({ file: archivePath, cwd: destination });
  }
}

// Move every extracted entry into `destination`, each file by an atomic rename, and the binary last:
// a reader that finds the binary (bin/basemind.js checks only that) finds its libraries beside it.
function placeEntries(source, destination, binaryName) {
  let binary = null;
  for (const entry of fs.readdirSync(source, { withFileTypes: true })) {
    const from = path.join(source, entry.name);
    const to = path.join(destination, entry.name);
    if (entry.isDirectory()) {
      fs.mkdirSync(to, { recursive: true });
      placeEntries(from, to, null);
    } else if (entry.name === binaryName) {
      binary = from;
    } else {
      fs.renameSync(from, to);
    }
  }
  if (binary) fs.renameSync(binary, path.join(destination, binaryName));
}

// Unpack the verified archive into a directory of this run's own and move the result into binDir only
// once it is complete. Extracting straight into binDir left a truncated binary behind an install that
// was killed mid-way, which every later run executed, and two first runs (`npx`) wrote the same files.
async function installFromArchive(archivePath, binDir, binaryName, staging, extract = extractArchive) {
  const unpacked = path.join(staging, "unpacked");
  fs.mkdirSync(unpacked);
  await extract(archivePath, unpacked);
  const staged = path.join(unpacked, binaryName);
  if (!fs.existsSync(staged)) {
    throw new Error(`binary ${binaryName} not found in ${path.basename(archivePath)}`);
  }
  if (os.type() !== "Windows_NT") {
    fs.chmodSync(staged, 0o755);
  }
  placeEntries(unpacked, binDir, binaryName);
}

async function installBinary() {
  let staging = null;
  try {
    const { assetName, archiveUrl, checksumsUrl } = getReleaseAssets();
    const binDir = path.join(__dirname, "bin");
    const binaryName = os.type() === "Windows_NT" ? "basemind.exe" : "basemind";
    const binaryPath = path.join(binDir, binaryName);

    if (!fs.existsSync(binDir)) {
      fs.mkdirSync(binDir, { recursive: true });
    }

    if (fs.existsSync(binaryPath)) {
      return;
    }

    // Inside binDir, so every rename into it stays on one filesystem.
    staging = fs.mkdtempSync(path.join(binDir, ".install-"));
    const archivePath = path.join(staging, assetName);

    console.log(`Downloading basemind binary from ${archiveUrl}...`);

    await retryWithBackoff(() => downloadWithRedirects(archiveUrl, archivePath));

    await verifyChecksum(archivePath, assetName, checksumsUrl);

    console.log("Extracting archive (binary + bundled libraries)...");

    await installFromArchive(archivePath, binDir, binaryName, staging);

    console.log("basemind binary installed successfully!");
  } catch (error) {
    console.error("Error installing basemind binary:", error.message);
    console.error(proxyHint());
    process.exitCode = 1;
  } finally {
    if (staging) fs.rmSync(staging, { recursive: true, force: true });
  }
}

if (require.main === module) {
  installBinary();
}

module.exports = {
  downloadWithRedirects,
  fetchTextWithRedirects,
  getPlatformTriple,
  installFromArchive,
  proxyFor,
  proxyHint,
};
