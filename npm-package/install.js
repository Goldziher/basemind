const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const https = require("node:https");
const crypto = require("node:crypto");
const { execFileSync } = require("node:child_process");

const { version } = require("./package.json");

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

function downloadWithRedirects(url, dest, maxRedirects = 5) {
  return new Promise((resolve, reject) => {
    const urlObj = nextUrl(url, maxRedirects, reject);
    if (!urlObj) return;

    const req = https.get(
      urlObj,
      {
        headers: {
          "User-Agent": "basemind-npm-wrapper",
        },
      },
      (res) => {
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
      },
    );

    req.on("error", reject);
    req.setTimeout(30000, () => {
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

    const req = https.get(
      urlObj,
      {
        headers: {
          "User-Agent": "basemind-npm-wrapper",
        },
      },
      (res) => {
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
      },
    );

    req.on("error", reject);
    req.setTimeout(30000, () => {
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
    process.exitCode = 1;
  } finally {
    if (staging) fs.rmSync(staging, { recursive: true, force: true });
  }
}

if (require.main === module) {
  installBinary();
}

module.exports = { downloadWithRedirects, fetchTextWithRedirects, getPlatformTriple, installFromArchive };
