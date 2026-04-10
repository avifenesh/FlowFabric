// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

fn main() {
    // Create 'dns_tests_enabled' configuration flag.
    // See README.md#dns-tests for setup instructions.
    println!("cargo::rustc-check-cfg=cfg(dns_tests_enabled)");
    if std::env::var("VALKEY_GLIDE_DNS_TESTS_ENABLED").is_ok() {
        println!("cargo:rustc-cfg=dns_tests_enabled");
    }
}
