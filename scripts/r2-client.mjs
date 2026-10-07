// The network half of publishing to the R2 bucket behind https://dl.mewrk.dev,
// shared by scripts/release-mirror.mjs (GitHub releases) and
// scripts/publish-components.mjs (Mewrk's own components). The pure half —
// bucket names, SigV4 signing, S3 credentials — is release-mirror-plan.mjs.
//
// Credentials come from the environment:
//   CLOUDFLARE_API_TOKEN   an API token with Workers R2 Storage: Edit (the S3
//                          credentials are derived from it) and, for the purge,
//                          Cache Purge on the mewrk.dev zone
//   CLOUDFLARE_ACCOUNT_ID  the account that owns the bucket
//   CLOUDFLARE_ZONE_ID     the mewrk.dev zone, for purging the CDN's copies
//   R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY  optional S3 credentials to use
//                          instead of deriving them from the API token

import { createHash } from "node:crypto";
import process from "node:process";

import { MIRROR, canonicalPath, credentialsFromApiToken, signS3Request } from "./release-mirror-plan.mjs";

export const TRANSFER_TIMEOUT = 20 * 60 * 1000;
export const API_TIMEOUT = 30 * 1000;

export const sha256Hex = (data) => createHash("sha256").update(data).digest("hex");
export const megabytes = (bytes) => `${(bytes / 1e6).toFixed(1)} MB`;

/**
 * The R2 and Cloudflare calls a publisher makes. `label` prefixes every line it
 * prints; `userAgent` is what it calls the public address with.
 */
export function createR2Client({ label, userAgent }) {
  /** A request retried twice on a network error or a 5xx, the failures a CI run meets by chance. */
  async function request(url, init = {}, timeout = API_TIMEOUT) {
    for (let attempt = 1; ; attempt += 1) {
      try {
        const response = await fetch(url, { ...init, signal: AbortSignal.timeout(timeout) });
        if (response.status < 500 || attempt === 3) return response;
        console.warn(`${label} ${init.method ?? "GET"} ${url}: HTTP ${response.status}, retrying`);
      } catch (error) {
        if (attempt === 3) throw new Error(`${init.method ?? "GET"} ${url}: ${error.message}`);
        console.warn(`${label} ${init.method ?? "GET"} ${url}: ${error.message}, retrying`);
      }
      await new Promise((resolve) => setTimeout(resolve, attempt * 5000));
    }
  }

  async function json(response, what) {
    if (!response.ok) throw new Error(`${what}: HTTP ${response.status} ${(await response.text()).slice(0, 300)}`);
    return response.json();
  }

  /** The token as pasted, without the stray whitespace a paste can carry: fetch trims it from a header, but the S3 secret hashes every byte. */
  function apiToken() {
    return (process.env.CLOUDFLARE_API_TOKEN ?? "").trim();
  }

  function cloudflareHeaders() {
    return { authorization: `Bearer ${apiToken()}`, "content-type": "application/json" };
  }

  /** The API token's id: a user token answers on /user, an account-owned one on its account. */
  async function apiTokenId() {
    const paths = ["user/tokens/verify", `accounts/${process.env.CLOUDFLARE_ACCOUNT_ID}/tokens/verify`];
    for (const path of paths) {
      const response = await request(`https://api.cloudflare.com/client/v4/${path}`, { headers: cloudflareHeaders() });
      const body = await response.json().catch(() => ({}));
      if (response.ok && body.success && body.result?.id) {
        if (body.result.status !== "active") throw new Error(`the Cloudflare API token is ${body.result.status}`);
        console.log(`${label} API token verified as ${path.startsWith("user") ? "a user" : "an account"} token`);
        return body.result.id;
      }
    }
    throw new Error("CLOUDFLARE_API_TOKEN does not verify as a user or account token");
  }

  async function s3Credentials() {
    if (process.env.R2_ACCESS_KEY_ID && process.env.R2_SECRET_ACCESS_KEY) {
      return { accessKeyId: process.env.R2_ACCESS_KEY_ID, secretAccessKey: process.env.R2_SECRET_ACCESS_KEY };
    }
    if (!apiToken()) throw new Error("set CLOUDFLARE_API_TOKEN (or R2_ACCESS_KEY_ID and R2_SECRET_ACCESS_KEY)");
    return credentialsFromApiToken(await apiTokenId(), apiToken());
  }

  function bucketUrl(key) {
    return `https://${process.env.CLOUDFLARE_ACCOUNT_ID}.r2.cloudflarestorage.com${canonicalPath(MIRROR.bucket, key)}`;
  }

  /** The stored object's size and recorded SHA-256, or null when there is none. */
  async function headObject(credentials, key) {
    const url = bucketUrl(key);
    const empty = sha256Hex("");
    // Uncompressed, or a text object comes back without its length (see checkPublic).
    const headers = { ...signS3Request({ method: "HEAD", url, payloadSha256: empty, credentials }), "accept-encoding": "identity" };
    const response = await request(url, { method: "HEAD", headers });
    if (response.status === 404) return null;
    if (!response.ok) throw new Error(`HEAD ${key}: HTTP ${response.status}${await s3ErrorDetail(credentials, key)}`);
    return { size: Number(response.headers.get("content-length")), sha256: response.headers.get("x-amz-meta-sha256") };
  }

  /** A HEAD answer has no body; the same request as a one-byte GET says why S3 refused it. */
  async function s3ErrorDetail(credentials, key) {
    const url = bucketUrl(key);
    const headers = signS3Request({ method: "GET", url, headers: { range: "bytes=0-0" }, payloadSha256: sha256Hex(""), credentials });
    const text = await (await request(url, { headers })).text().catch(() => "");
    const field = (name) => new RegExp(`<${name}>([^<]*)</${name}>`).exec(text)?.[1];
    return field("Code") ? ` (${field("Code")}: ${field("Message") ?? ""})` : "";
  }

  /** Uploads `bytes` signed with their SHA-256, so R2 stores them only if they arrive intact. */
  async function putObject(credentials, key, bytes, sha256, headers) {
    const url = bucketUrl(key);
    const signed = signS3Request({ method: "PUT", url, headers: { ...headers, "x-amz-meta-sha256": sha256 }, payloadSha256: sha256, credentials });
    const response = await request(url, { method: "PUT", headers: signed, body: bytes }, TRANSFER_TIMEOUT);
    if (!response.ok) throw new Error(`PUT ${key}: HTTP ${response.status} ${(await response.text()).slice(0, 300)}`);
  }

  async function purge(urls) {
    const zone = process.env.CLOUDFLARE_ZONE_ID;
    if (!zone) throw new Error(`CLOUDFLARE_ZONE_ID is not set, so the CDN may still serve the old copy of: ${urls.join(", ")}`);
    const body = await json(
      await request(`https://api.cloudflare.com/client/v4/zones/${zone}/purge_cache`, { method: "POST", headers: cloudflareHeaders(), body: JSON.stringify({ files: urls }) }),
      "purging the CDN cache"
    );
    if (!body.success) throw new Error(`purging the CDN cache: ${JSON.stringify(body.errors)}`);
  }

  /**
   * The public address must serve the stored file, which also proves the custom domain works. A
   * small file is read whole and hashed; a large one must report its size. Uncompressed, because
   * Cloudflare compresses text for a client that asks and then sends no length.
   */
  async function checkPublic(url, size, sha256) {
    const small = size <= 1e6;
    const headers = { "user-agent": userAgent, "accept-encoding": "identity" };
    const response = await request(url, { method: small ? "GET" : "HEAD", headers });
    if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
    if (small) {
      const actual = sha256Hex(Buffer.from(await response.arrayBuffer()));
      if (actual !== sha256) throw new Error(`${url}: serves SHA-256 ${actual} instead of ${sha256}`);
      return;
    }
    const length = Number(response.headers.get("content-length"));
    if (length !== size) throw new Error(`${url}: reports ${length} bytes instead of ${size}`);
  }

  return { request, json, s3Credentials, headObject, putObject, purge, checkPublic };
}
