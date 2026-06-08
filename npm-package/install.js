const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const https = require("node:https");
const http = require("node:http");
const tar = require("tar");
const AdmZip = require("adm-zip");

const { version } = require("./package.json");

function getPlatformTriple() {
  const type = os.type();
  const arch = os.arch();

  if (type === "Windows_NT") {
    if (arch === "x64") return "x86_64-pc-windows-gnu";
    if (arch === "ia32") throw new Error("32-bit Windows is not supported");
  }

  if (type === "Linux") {
    if (arch === "x64") return "x86_64-unknown-linux-gnu";
    if (arch === "arm64") return "aarch64-unknown-linux-gnu";
    return "x86_64-unknown-linux-gnu";
  }

  if (type === "Darwin") {
    if (arch === "x64") return "x86_64-apple-darwin";
    if (arch === "arm64") return "aarch64-apple-darwin";
    return "x86_64-apple-darwin";
  }

  throw new Error(`Unsupported platform: ${type} ${arch}`);
}

function getBinaryUrl() {
  const platform = getPlatformTriple();
  const baseUrl = `https://github.com/Goldziher/gitmind/releases/download/v${version}`;
  const ext = platform.includes("windows") ? "zip" : "tar.gz";
  return `${baseUrl}/gitmind-${platform}.${ext}`;
}

function downloadWithRedirects(url, dest, maxRedirects = 5) {
  return new Promise((resolve, reject) => {
    if (maxRedirects <= 0) {
      return reject(new Error("Too many redirects"));
    }

    const urlObj = new URL(url);
    const client = urlObj.protocol === "https:" ? https : http;

    const req = client.get(
      url,
      {
        headers: {
          "User-Agent": "gitmind-npm-wrapper",
        },
      },
      (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          return downloadWithRedirects(res.headers.location, dest, maxRedirects - 1)
            .then(resolve)
            .catch(reject);
        }

        if (res.statusCode !== 200) {
          return reject(new Error(`HTTP ${res.statusCode}: ${res.statusMessage}`));
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

async function installBinary() {
  try {
    const url = getBinaryUrl();
    const isZip = url.endsWith(".zip");
    const binDir = path.join(__dirname, "bin");
    const archivePath = path.join(binDir, isZip ? "gitmind.zip" : "gitmind.tar.gz");
    const binaryName = os.type() === "Windows_NT" ? "gitmind.exe" : "gitmind";
    const binaryPath = path.join(binDir, binaryName);

    if (!fs.existsSync(binDir)) {
      fs.mkdirSync(binDir, { recursive: true });
    }

    if (fs.existsSync(binaryPath)) {
      return;
    }

    console.log(`Downloading gitmind binary from ${url}...`);

    await downloadWithRedirects(url, archivePath);

    console.log("Extracting binary...");

    if (isZip) {
      const zip = new AdmZip(archivePath);
      const entry = zip.getEntries().find((e) => e.entryName.endsWith(binaryName));
      if (!entry) {
        throw new Error("Binary not found in downloaded archive");
      }
      zip.extractEntryTo(entry, binDir, false, true);
    } else {
      await tar.extract({
        file: archivePath,
        cwd: binDir,
        filter: (entryPath) => entryPath.endsWith(binaryName),
      });
    }

    fs.unlinkSync(archivePath);

    if (os.type() !== "Windows_NT") {
      fs.chmodSync(binaryPath, 0o755);
    }

    console.log("gitmind binary installed successfully!");
  } catch (error) {
    console.error("Error installing gitmind binary:", error.message);
    process.exit(1);
  }
}

installBinary();
