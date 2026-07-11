//! Root-only smoke test for [`umf_networking::ContainerNet`].
//!
//! Creates a throwaway network namespace (`unshare --net`), wires NAT'd egress
//! into it via `ContainerNet::setup`, then proves the namespace can reach the
//! outside world (ICMP + a TCP/DNS lookup) from *inside* the netns. Finally it
//! drops the guard and asserts the host veth and nft table are gone.
//!
//! Run as root (the veth/setns/nft operations need `CAP_NET_ADMIN`):
//!
//! ```text
//! sudo -E cargo run -p umf-networking --example egress_smoke
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::process::Command;

use umf_networking::{ContainerNet, NetOptions};

fn nsenter(pid: u32, args: &[&str]) -> Result<std::process::Output, std::io::Error> {
    Command::new("nsenter")
        .arg("-t")
        .arg(pid.to_string())
        .arg("-n")
        .arg("--")
        .args(args)
        .output()
}

fn main() -> Result<(), Box<dyn Error>> {
    // A process living in a fresh net namespace, doing nothing for 5 minutes.
    let mut child = Command::new("unshare")
        .args(["--net", "sleep", "300"])
        .spawn()?;
    let pid = child.id();
    println!("[*] netns holder pid = {pid}");

    // Give the kernel a beat to materialise /proc/<pid>/ns/net.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Captured from the live network before teardown so the post-drop
    // assertions check the *actual* block-allocated names (the veth/table are
    // named by the allocated subnet block, not the PID).
    let mut observed: Option<(String, String)> = None;
    let result = (|| -> Result<(), Box<dyn Error>> {
        let net = ContainerNet::setup(pid, &NetOptions::default())?;
        let host_if = net.host_ifname().to_string();
        // The nft table mirrors the veth's block: `umfv{block}h` → `umf-nat-{block}`.
        let block = host_if
            .trim_start_matches("umfv")
            .trim_end_matches('h')
            .to_string();
        observed = Some((host_if.clone(), format!("umf-nat-{block}")));
        println!(
            "[*] setup ok: host_if={} container_ip={}",
            host_if,
            net.container_ip()
        );

        // 1. ICMP egress to a public anycast address. Skippable via
        //    UMF_EGRESS_SKIP_ICMP: some networks (GitHub-hosted CI runners)
        //    drop outbound raw ICMP to public IPs, which fails this probe even
        //    when the NAT path is fine. DNS (UDP/53) and TCP/443 below prove
        //    egress without it, so they stay mandatory.
        if std::env::var_os("UMF_EGRESS_SKIP_ICMP").is_some() {
            println!("[~] ICMP egress check skipped (UMF_EGRESS_SKIP_ICMP set)");
        } else {
            let ping = nsenter(pid, &["ping", "-c", "2", "-W", "3", "1.1.1.1"])?;
            println!(
                "[ping 1.1.1.1] rc={:?}\n{}",
                ping.status.code(),
                String::from_utf8_lossy(&ping.stdout)
            );
            if !ping.status.success() {
                return Err("ICMP egress FAILED".into());
            }
            println!("[+] ICMP egress works");
        }

        // 2. UDP/53 egress: query a public resolver directly. We pass the
        //    resolver explicitly (1.1.1.1) rather than relying on the host's
        //    /etc/resolv.conf — `nsenter -n` keeps the host mount ns, and a
        //    systemd-resolved host points resolv.conf at 127.0.0.53 (loopback,
        //    which is the netns's *own* loopback here). Picking the right
        //    in-container resolv.conf is umf-engine's job, not umf-networking's.
        let dns = nsenter(pid, &["nslookup", "-timeout=5", "example.com", "1.1.1.1"])?;
        println!(
            "[nslookup @1.1.1.1] rc={:?}\n{}",
            dns.status.code(),
            String::from_utf8_lossy(&dns.stdout)
        );
        if !dns.status.success() {
            return Err("UDP/53 (DNS) egress FAILED".into());
        }
        println!("[+] UDP/53 (DNS) egress works");

        // 3. TCP/443 egress: HTTPS to Cloudflare's 1.1.1.1 (its cert carries the
        //    IP in a SAN, so no hostname/DNS needed) — proves a full TLS round
        //    trip out through the NAT.
        let curl = nsenter(
            pid,
            &[
                "curl",
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "10",
                "https://1.1.1.1/",
            ],
        )?;
        println!(
            "[curl https://1.1.1.1] rc={:?} http={} err={}",
            curl.status.code(),
            String::from_utf8_lossy(&curl.stdout),
            String::from_utf8_lossy(&curl.stderr)
        );
        if !curl.status.success() {
            return Err("TCP/443 egress FAILED".into());
        }
        println!("[+] TCP/443 (HTTPS) egress works");

        // Hold the guard until here; drop it to tear down.
        drop(net);
        Ok(())
    })();

    // Always reap the child.
    let _ = child.kill();
    let _ = child.wait();

    result?;

    // Teardown assertions: the host veth + nft table (the real block-named
    // ones captured at setup) must be gone.
    let (host_if, table) = observed.ok_or("setup never ran")?;
    let link = Command::new("ip")
        .args(["link", "show", &host_if])
        .output()?;
    if link.status.success() {
        return Err(format!("teardown leaked host veth {host_if}").into());
    }
    let nft = Command::new("nft")
        .args(["list", "table", "inet", &table])
        .output()?;
    if nft.status.success() {
        return Err(format!("teardown leaked nft table {table}").into());
    }
    println!("[+] teardown clean: no leaked veth, no leaked nft table");

    println!("\n=== egress smoke test PASSED ===");
    Ok(())
}
