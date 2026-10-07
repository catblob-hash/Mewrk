import assert from "node:assert/strict";
import test from "node:test";
import {
  extractCopyright,
  isClaudeAgentPackage,
  licenseFamilies,
  normalDependencyIds,
  packageLicense,
  productionLockEntries,
  textMatchesFamily
} from "../third-party-licenses.mjs";

test("classifies SPDX expressions, legacy Cargo slashes, and exceptions", () => {
  assert.deepEqual(licenseFamilies("(MIT OR Apache-2.0) AND Unicode-3.0"), ["Apache-2.0", "MIT", "Unicode-3.0"]);
  assert.deepEqual(licenseFamilies("MIT/Apache-2.0"), ["Apache-2.0", "MIT"]);
  assert.deepEqual(licenseFamilies("Apache-2.0 WITH LLVM-exception OR MIT"), ["Apache-2.0", "LLVM-exception", "MIT"]);
  assert.deepEqual(licenseFamilies("MIT OR MIT"), ["MIT"]);
});

test("commercial license pointers are never reclassified as MIT", () => {
  assert.deepEqual(licenseFamilies("SEE LICENSE IN README.md"), []);
  assert.equal(packageLicense({ license: "SEE LICENSE IN README.md" }, "sdk"), "SEE LICENSE IN README.md");
});

test("supports legacy npm license arrays and Cargo license_file", () => {
  assert.equal(packageLicense({ licenses: [{ type: "MIT" }, { type: "BSD-3-Clause" }] }, "legacy"), "MIT OR BSD-3-Clause");
  assert.equal(packageLicense({ license_file: "COPYING" }, "crate"), "SEE LICENSE IN COPYING");
  assert.throws(() => packageLicense({}, "missing@1.0.0"), /No determinable license for missing@1\.0\.0/);
  assert.throws(() => packageLicense({ licenses: [{}] }, "broken"), /No determinable license/);
});

test("extracts all attribution lines without license prose or template placeholders", () => {
  const text = "The above copyright notice must be retained.\r\nCopyright (c) 2024 Alice\r\nCopyright 2025 Bob\r\nCopyright (c) 2024 Alice\r\nCopyright [yyyy] [name of copyright owner]";
  assert.equal(extractCopyright(text), "Copyright (c) 2024 Alice; Copyright 2025 Bob");
  assert.equal(extractCopyright("Copyright (c) 2020\nExample Foundation\n"), "Copyright (c) 2020 Example Foundation");
});

test("accepts a bare © line as an attribution, but not a numbered (c) clause", () => {
  assert.equal(extractCopyright("© Anthropic PBC. All rights reserved."), "© Anthropic PBC. All rights reserved.");
  assert.equal(extractCopyright("(c) The licensee shall not remove notices."), "no copyright line in package");
});

test("falls back to authors when license prose contains no copyright owner", () => {
  assert.equal(extractCopyright("1. Copyright and Related Rights", { name: "Alice", email: "alice@example.test" }), "Alice <alice@example.test>");
  assert.equal(extractCopyright("", ["Alice", "Bob"], "crate"), "Alice; Bob");
  assert.equal(extractCopyright(""), "no copyright line in package");
  assert.equal(extractCopyright("", [], "crate"), "no copyright line in crate");
});

test("requires actual grant text rather than an identifier mention", () => {
  assert.equal(textMatchesFamily("See the Apache License Version 2.0 online", "Apache-2.0"), false);
  assert.equal(textMatchesFamily("This SDK is governed by Commercial Terms, not MIT.", "MIT"), false);
  const mit = "Permission is hereby granted, free of charge\nThe above copyright notice and this permission notice\nTHE SOFTWARE IS PROVIDED";
  assert.equal(textMatchesFamily(mit, "MIT"), true);
  assert.equal(textMatchesFamily(mit, "MIT-0"), false);
  const isc = "Permission to use, copy, modify, and/or distribute\nprovided the above copyright notice and this permission notice appear\nTHE SOFTWARE IS PROVIDED";
  assert.equal(textMatchesFamily(isc, "ISC"), true);
  assert.equal(textMatchesFamily(isc, "0BSD"), false);
  const ijg = "The authors make NO WARRANTY or representation\nPermission is hereby granted to use, copy, modify, and distribute this\nsoftware (or portions thereof) for any purpose, without fee\nthe accompanying documentation must state that \"this software is based in part on the work of\nthe Independent JPEG Group\".";
  assert.equal(textMatchesFamily(ijg, "IJG"), true);
  assert.equal(textMatchesFamily("Uses code of the Independent JPEG Group (IJG).", "IJG"), false);
  assert.deepEqual(licenseFamilies("(MIT OR Apache-2.0) AND IJG"), ["Apache-2.0", "IJG", "MIT"]);
});

test("walks only normal edges, including proc-macros, without build/dev descendants", () => {
  const edge = (pkg, ...kinds) => ({ pkg, dep_kinds: kinds.map((kind) => ({ kind })) });
  const metadata = { workspace_members: ["app", "core"], resolve: { nodes: [
    { id: "app", deps: [edge("normal", null), edge("build", "build"), edge("dev", "dev"), edge("mixed", "build", null), edge("macro", null)] },
    { id: "core", deps: [edge("normal", null)] },
    { id: "normal", deps: [edge("transitive", null), edge("build", "build")] },
    { id: "mixed", deps: [] },
    { id: "macro", deps: [] },
    { id: "transitive", deps: [edge("normal", null)] }
  ] } };
  assert.deepEqual([...normalDependencyIds(metadata)].sort(), ["macro", "mixed", "normal", "transitive"]);
});

test("the Claude Agent SDK and its platform packages are recognised, the AI SDK's Anthropic provider is not", () => {
  assert.equal(isClaudeAgentPackage("@anthropic-ai/claude-agent-sdk"), true);
  assert.equal(isClaudeAgentPackage("@anthropic-ai/claude-agent-sdk-win32-x64"), true);
  assert.equal(isClaudeAgentPackage("@anthropic-ai/claude-agent-sdk-linux-arm64-musl"), true);
  assert.equal(isClaudeAgentPackage("@anthropic-ai/sdk"), false);
  assert.equal(isClaudeAgentPackage("@ai-sdk/anthropic"), false);
});

test("the inventory skips the root and dev-only entries, including the SDK and its platform packages", () => {
  const lock = { packages: {
    "": { name: "mewrk-aisdk-service" },
    "node_modules/ai": { version: "7.0.83" },
    "node_modules/@ai-sdk/anthropic": { version: "4.0.44" },
    "node_modules/@ai-sdk/provider": { version: "4.0.0" },
    "node_modules/@anthropic-ai/claude-agent-sdk": { version: "0.3.284", dev: true },
    "node_modules/@anthropic-ai/claude-agent-sdk-win32-x64": { version: "0.3.284", dev: true, optional: true },
    "node_modules/@anthropic-ai/claude-agent-sdk-darwin-arm64": { version: "0.3.284", dev: true, optional: true },
    "node_modules/@anthropic-ai/sdk": { version: "0.124.0", dev: true, peer: true },
    "node_modules/esbuild": { version: "0.25.0", dev: true }
  } };
  assert.deepEqual(productionLockEntries(lock).map(([location, , name]) => [location, name]), [
    ["node_modules/@ai-sdk/anthropic", "@ai-sdk/anthropic"],
    ["node_modules/@ai-sdk/provider", "@ai-sdk/provider"],
    ["node_modules/ai", "ai"]
  ]);
});

test("the inventory refuses a Claude Agent SDK package that is a production dependency again", () => {
  for (const name of ["@anthropic-ai/claude-agent-sdk", "@anthropic-ai/claude-agent-sdk-win32-x64"]) {
    const lock = { packages: { "": {}, [`node_modules/${name}`]: { version: "0.3.284" } } };
    assert.throws(() => productionLockEntries(lock), /production dependency of aisdk-service again/);
  }
  // A nested copy under another package is caught by its name too.
  const nested = { packages: { "": {}, "node_modules/some-wrapper/node_modules/@anthropic-ai/claude-agent-sdk": { version: "0.3.284" } } };
  assert.throws(() => productionLockEntries(nested), /production dependency of aisdk-service again/);
});
