import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { parse as parseToml } from "smol-toml";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const ajv = new Ajv2020({ allErrors: true, strict: true, validateFormats: true });
addFormats(ajv, { mode: "full" });

function readJson(relativePath) {
  return JSON.parse(readFileSync(join(root, relativePath), "utf8"));
}

function readToml(relativePath) {
  return parseToml(readFileSync(join(root, relativePath), "utf8"));
}

function compile(relativePath) {
  return ajv.compile(readJson(relativePath));
}

function describeErrors(validate) {
  return ajv.errorsText(validate.errors, { separator: "\n  " });
}

function expectValid(label, validate, value) {
  if (!validate(value)) {
    throw new Error(`${label} did not match its schema:\n  ${describeErrors(validate)}`);
  }
  process.stdout.write(`schema-valid: ${label}\n`);
}

function expectInvalid(label, validate, value) {
  if (validate(value)) {
    throw new Error(`${label} unexpectedly matched its schema`);
  }
  process.stdout.write(`schema-rejected: ${label}\n`);
}

function clone(value) {
  return JSON.parse(JSON.stringify(value));
}

const configSchema = compile("schemas/config.schema.json");
const developmentConfig = readToml("config/xanhtab.toml");
const productionConfig = readToml("config/xanhtab.production.toml");
expectValid("development config", configSchema, developmentConfig);
expectValid("production config", configSchema, productionConfig);

const configWithUnknownKey = clone(developmentConfig);
configWithUnknownKey.server.unreviewed_option = true;
expectInvalid("config unknown key", configSchema, configWithUnknownKey);

const productionConfigWithoutTlsKey = clone(productionConfig);
delete productionConfigWithoutTlsKey.server.tls_key;
expectInvalid("production config without TLS key", configSchema, productionConfigWithoutTlsKey);

const configWithRemoteManagedProxy = clone(productionConfig);
configWithRemoteManagedProxy.network.warp_proxy = "socks5h://192.0.2.1:40000";
expectInvalid("managed proxy outside loopback", configSchema, configWithRemoteManagedProxy);

const configWithUnsafeWireGuardName = clone(productionConfig);
configWithUnsafeWireGuardName.network.wireguard_config = "/etc/xanhtab/secrets/home.conf";
expectInvalid("WireGuard interface outside dedicated wg0", configSchema, configWithUnsafeWireGuardName);

const remoteConfigSchema = compile("schemas/remote-config.schema.json");
const remoteConfig = readJson("tests/fixtures/remote-config/config.json");
expectValid("Git-backed public config", remoteConfigSchema, remoteConfig);

const remoteConfigWithSecret = clone(remoteConfig);
remoteConfigWithSecret.proxy_password = "must-not-be-public";
expectInvalid("public config secret field", remoteConfigSchema, remoteConfigWithSecret);

const bookmarksSchema = compile("schemas/bookmarks.schema.json");
const bookmarks = readJson("tests/fixtures/remote-config/bookmarks.json");
expectValid("Git-backed bookmarks", bookmarksSchema, bookmarks);

const bookmarksWithScriptUrl = clone(bookmarks);
bookmarksWithScriptUrl.bookmarks[0].url = "javascript:alert(1)";
expectInvalid("bookmark script URL", bookmarksSchema, bookmarksWithScriptUrl);

const blocklistMetadataSchema = compile("schemas/blocklist-metadata.schema.json");
const blocklistMetadata = readJson("tests/fixtures/remote-config/blocklist-metadata.json");
expectValid("blocklist provenance metadata", blocklistMetadataSchema, blocklistMetadata);

const blocklistMetadataWithCredentialUrl = clone(blocklistMetadata);
blocklistMetadataWithCredentialUrl.sources[0].url = "https://user@lists.example/blocklist.txt";
expectInvalid("blocklist credential URL", blocklistMetadataSchema, blocklistMetadataWithCredentialUrl);

const blocklistMetadataExternalOnly = clone(blocklistMetadata);
blocklistMetadataExternalOnly.sources[0].redistribution = "external_fetch_only";
expectValid("external-fetch-only blocklist metadata", blocklistMetadataSchema, blocklistMetadataExternalOnly);

const blocklistMetadataWithoutLicense = clone(blocklistMetadata);
delete blocklistMetadataWithoutLicense.sources[0].license;
expectInvalid("blocklist metadata without license", blocklistMetadataSchema, blocklistMetadataWithoutLicense);

const blocklistMetadataWithCredentialLicenseUrl = clone(blocklistMetadata);
blocklistMetadataWithCredentialLicenseUrl.sources[0].license_url = "https://user@lists.example/license";
expectInvalid("blocklist credential license URL", blocklistMetadataSchema, blocklistMetadataWithCredentialLicenseUrl);

const burnAuditSchema = compile("schemas/burn-audit.schema.json");
const burnAudit = readJson("tests/fixtures/burn-audit-pass.json");
expectValid("passing burn audit", burnAuditSchema, burnAudit);

const burnAuditWithResidue = clone(burnAudit);
burnAuditWithResidue.observations.runtime_entries = 1;
expectInvalid("passing burn audit with runtime residue", burnAuditSchema, burnAuditWithResidue);

const encoderProbeSchema = compile("schemas/encoder-probe.schema.json");
const encoderProbe = readJson("benchmarks/x0-encoder-2026-08-20/summary.json");
expectValid("published X0 encoder evidence", encoderProbeSchema, encoderProbe);

const encoderProbeWithUnknownField = clone(encoderProbe);
encoderProbeWithUnknownField.profiles[0].unreviewed_measurement = 1;
expectInvalid("encoder evidence unknown field", encoderProbeSchema, encoderProbeWithUnknownField);

const releaseManifestSchema = compile("schemas/release-manifest.schema.json");
const releaseManifest = JSON.parse(
  execFileSync(
    join(root, "scripts/render-release-manifest.sh"),
    [
      "0.1.0-dev.1",
      "xanhtab-0.1.0-dev.1-linux-aarch64.tar.zst",
      "https://releases.example/xanhtab-0.1.0-dev.1-linux-aarch64.tar.zst",
      "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "2.48.3-1",
      "1.26.2",
      "0.14.0"
    ],
    { encoding: "utf8" }
  )
);
expectValid("rendered release manifest", releaseManifestSchema, releaseManifest);

const releaseManifestWithInvalidChecksum = clone(releaseManifest);
releaseManifestWithInvalidChecksum.artifacts[0].sha256 = "not-a-sha256";
expectInvalid("release manifest invalid checksum", releaseManifestSchema, releaseManifestWithInvalidChecksum);

const releaseManifestWithDuplicateArtifact = clone(releaseManifest);
releaseManifestWithDuplicateArtifact.artifacts.push(clone(releaseManifest.artifacts[0]));
expectInvalid("release manifest duplicate ARM64 artifact", releaseManifestSchema, releaseManifestWithDuplicateArtifact);

const releaseManifestWithCredentialUrl = clone(releaseManifest);
releaseManifestWithCredentialUrl.artifacts[0].url = "https://user@releases.example/xanhtab.tar.zst";
expectInvalid("release manifest credential URL", releaseManifestSchema, releaseManifestWithCredentialUrl);
