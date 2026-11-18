// The npm wrapper's installer refuses a platform it has no binary for, and a binary reaches bin/ only
// whole, with its libraries beside it.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { test } = require("node:test");

const { getPlatformTriple, installFromArchive } = require("../npm-package/install.js");

const BINARY = os.type() === "Windows_NT" ? "basemind.exe" : "basemind";

function scratch() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "basemind-npm-staging-"));
  const binDir = path.join(root, "bin");
  fs.mkdirSync(binDir);
  const staging = fs.mkdtempSync(path.join(binDir, ".install-"));
  return { root, binDir, staging };
}

test("a Linux architecture with no published binary is refused", () => {
  assert.equal(getPlatformTriple("Linux", "x64"), "x86_64-unknown-linux-gnu");
  assert.equal(getPlatformTriple("Linux", "arm64"), "aarch64-unknown-linux-gnu");
  for (const arch of ["riscv64", "ppc64", "s390x", "ia32"]) {
    assert.throws(() => getPlatformTriple("Linux", arch), /Linux on .* is not supported/u, arch);
  }
});

test("an extraction that dies part-way leaves no binary in bin/", async () => {
  const { root, binDir, staging } = scratch();
  try {
    const dies = async (_archive, destination) => {
      fs.writeFileSync(path.join(destination, BINARY), "truncated");
      throw new Error("killed mid-extract");
    };
    await assert.rejects(installFromArchive("basemind.tar.gz", binDir, BINARY, staging, dies), /killed/u);
    assert.equal(fs.existsSync(path.join(binDir, BINARY)), false);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a complete extraction moves the binary and its libraries into bin/", async () => {
  const { root, binDir, staging } = scratch();
  try {
    fs.mkdirSync(path.join(binDir, "lib"));
    fs.writeFileSync(path.join(binDir, "lib", "libold.so"), "from an earlier install");
    const completes = async (_archive, destination) => {
      fs.writeFileSync(path.join(destination, BINARY), "binary");
      fs.mkdirSync(path.join(destination, "lib"));
      fs.writeFileSync(path.join(destination, "lib", "libonnxruntime.so"), "library");
    };
    await installFromArchive("basemind.tar.gz", binDir, BINARY, staging, completes);
    assert.equal(fs.readFileSync(path.join(binDir, BINARY), "utf8"), "binary");
    assert.equal(fs.readFileSync(path.join(binDir, "lib", "libonnxruntime.so"), "utf8"), "library");
    assert.equal(fs.existsSync(path.join(binDir, "lib", "libold.so")), true);
    if (os.type() !== "Windows_NT") {
      assert.equal(fs.statSync(path.join(binDir, BINARY)).mode & 0o111, 0o111);
    }
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("an archive without the binary installs nothing", async () => {
  const { root, binDir, staging } = scratch();
  try {
    const libraryOnly = async (_archive, destination) => {
      fs.writeFileSync(path.join(destination, "LICENSE"), "license");
    };
    await assert.rejects(
      installFromArchive("basemind.tar.gz", binDir, BINARY, staging, libraryOnly),
      /not found in basemind\.tar\.gz/u,
    );
    assert.deepEqual(
      fs.readdirSync(binDir).filter((name) => !name.startsWith(".install-")),
      [],
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
