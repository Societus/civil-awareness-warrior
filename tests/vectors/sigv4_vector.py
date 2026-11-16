#!/usr/bin/env python3
"""Independent SigV4 signature computation for the Rust test vector.

Deliberately NOT a port of the Rust code: computes the canonical request,
string-to-sign and signature with plain hashlib/hmac so the Rust test in
src/staging.rs verifies against a second implementation of the same algorithm.

Writes the signature to tests/vectors/sigv4_signature.txt.
"""
import hashlib
import hmac
import pathlib

payload = b"landhunter-test-payload"
payload_hash = hashlib.sha256(payload).hexdigest()

host = "abc123.r2.cloudflarestorage.com"
canonical_uri = "/lh-stage/stage/raw/ingest/WI/55011/l1/pages/0000.zst"
amz_date = "20261116T120000Z"
date_stamp = "20261116"
region = "auto"
service = "s3"
access_key = "AKIDEXAMPLE"
secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"

canonical_headers = (
    f"host:{host}\n"
    f"x-amz-content-sha256:{payload_hash}\n"
    f"x-amz-date:{amz_date}\n"
)
signed_headers = "host;x-amz-content-sha256;x-amz-date"

canonical_request = (
    f"PUT\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
)

scope = f"{date_stamp}/{region}/{service}/aws4_request"
string_to_sign = (
    f"AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n"
    f"{hashlib.sha256(canonical_request.encode()).hexdigest()}"
)


def hm(key: bytes, msg: bytes) -> bytes:
    return hmac.new(key, msg, hashlib.sha256).digest()


k_date = hm(("AWS4" + secret).encode(), date_stamp.encode())
k_region = hm(k_date, region.encode())
k_service = hm(k_region, service.encode())
k_signing = hm(k_service, b"aws4_request")

signature = hmac.new(k_signing, string_to_sign.encode(), hashlib.sha256).hexdigest()

out = pathlib.Path(__file__).parent / "sigv4_signature.txt"
out.write_text(signature + "\n")
print("payload_hash :", payload_hash)
print("canonical    :", canonical_request)
print("string2sign  :", string_to_sign)
print("signature    :", signature)
print("written to   :", out)
