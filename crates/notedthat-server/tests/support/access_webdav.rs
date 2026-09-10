use reqwest::{Method, StatusCode};

use super::{
    access_env::{DAV_PASS, DAV_USER, PRIVATE_BODY, PRIVATE_KB, PROPFIND, PUBLIC_BODY, PUBLIC_KB},
    access_server::ServerInstance,
    access_wire::{method, wire},
};

pub async fn verify(client: &reqwest::Client, server: &ServerInstance) {
    verify_public_reads(client, server).await;
    verify_credentials_and_writes(client, server).await;
}

async fn verify_public_reads(client: &reqwest::Client, server: &ServerInstance) {
    let root = wire(
        "WebDAV anonymous root PROPFIND",
        client
            .request(method("PROPFIND"), format!("{}/", server.dav_url))
            .header("Depth", "1")
            .body(PROPFIND)
            .send()
            .await
            .expect("root PROPFIND"),
    )
    .await;
    assert_eq!(root.status.as_u16(), 207);
    assert!(root.text().contains("/webdav/"));
    assert!(root.text().contains("/webdav/public/"));
    assert!(root.text().contains(PUBLIC_KB));
    assert!(!root.text().contains(PRIVATE_KB));
    let kb = wire(
        "WebDAV anonymous KB PROPFIND",
        client
            .request(
                method("PROPFIND"),
                format!("{}/{PUBLIC_KB}", server.dav_url),
            )
            .header("Depth", "1")
            .body(PROPFIND)
            .send()
            .await
            .expect("KB PROPFIND"),
    )
    .await;
    assert_eq!(kb.status.as_u16(), 207);
    assert!(kb.text().contains("/webdav/public/public.md"));
    assert!(kb.text().contains("public.md"));
    assert!(!kb.text().contains(".notedthat"));
    let content = wire(
        "WebDAV anonymous content",
        client
            .get(format!("{}/{PUBLIC_KB}/public.md", server.dav_url))
            .send()
            .await
            .expect("DAV GET"),
    )
    .await;
    assert_eq!(content.text(), PUBLIC_BODY);
    let options = wire(
        "WebDAV anonymous OPTIONS",
        client
            .request(
                Method::OPTIONS,
                format!("{}/{PUBLIC_KB}/public.md", server.dav_url),
            )
            .send()
            .await
            .expect("DAV OPTIONS"),
    )
    .await;
    assert_eq!(options.status, StatusCode::NO_CONTENT);
    let allow = options.headers["allow"].to_str().expect("Allow header");
    assert!(allow.contains("GET") && allow.contains("HEAD") && allow.contains("PROPFIND"));
    assert!(!allow.contains("PUT") && !allow.contains("DELETE"));
}

async fn verify_credentials_and_writes(client: &reqwest::Client, server: &ServerInstance) {
    let invalid = wire(
        "WebDAV malformed Basic",
        client
            .get(format!("{}/{PUBLIC_KB}/public.md", server.dav_url))
            .header("Authorization", "Basic !!!")
            .send()
            .await
            .expect("malformed Basic"),
    )
    .await;
    assert_eq!(invalid.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        invalid.headers["www-authenticate"],
        "Basic realm=\"NotedThat\""
    );
    assert!(invalid.text().contains("valid credentials are required"));
    assert!(
        invalid
            .text()
            .contains("invalid credentials are not treated as anonymous")
    );

    let private = wire(
        "WebDAV authenticated private content",
        client
            .get(format!("{}/{PRIVATE_KB}/private.md", server.dav_url))
            .basic_auth(DAV_USER, Some(DAV_PASS))
            .send()
            .await
            .expect("private DAV"),
    )
    .await;
    assert_eq!(private.text(), PRIVATE_BODY);
    for write_method in [
        "PUT",
        "DELETE",
        "MKCOL",
        "MOVE",
        "COPY",
        "PROPPATCH",
        "LOCK",
        "UNLOCK",
    ] {
        let denied = wire(
            &format!("WebDAV anonymous {write_method}"),
            client
                .request(
                    method(write_method),
                    format!("{}/{PUBLIC_KB}/public.md", server.dav_url),
                )
                .header("Destination", "http://[malformed-injection")
                .body("attacker overwrite")
                .send()
                .await
                .expect("denied DAV write"),
        )
        .await;
        assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
        assert!(denied.headers.contains_key("www-authenticate"));
    }
    let internal = wire(
        "WebDAV anonymous hidden internal content",
        client
            .get(format!("{}/{PUBLIC_KB}/.notedthat/leak.md", server.dav_url))
            .send()
            .await
            .expect("hidden DAV content"),
    )
    .await;
    assert_eq!(internal.status, StatusCode::UNAUTHORIZED);
    let unchanged = wire(
        "WebDAV authenticated unchanged after denied writes",
        client
            .get(format!("{}/{PUBLIC_KB}/public.md", server.dav_url))
            .basic_auth(DAV_USER, Some(DAV_PASS))
            .send()
            .await
            .expect("unchanged DAV"),
    )
    .await;
    assert_eq!(unchanged.text(), PUBLIC_BODY);
}
