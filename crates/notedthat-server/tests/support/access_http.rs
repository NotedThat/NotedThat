use reqwest::StatusCode;

use super::{
    access_env::{API_TOKEN, INTERNAL_BODY, PRIVATE_BODY, PRIVATE_KB, PUBLIC_BODY, PUBLIC_KB},
    access_server::ServerInstance,
    access_wire::{assert_http_401, method, search, wire},
};

pub async fn verify(client: &reqwest::Client, server: &ServerInstance) {
    verify_public_reads(client, server).await;
    verify_credentials_and_writes(client, server).await;
}

async fn verify_public_reads(client: &reqwest::Client, server: &ServerInstance) {
    let llms = wire(
        "HTTP anonymous llms.txt",
        client
            .get(format!("{}/llms.txt", server.http_url))
            .send()
            .await
            .expect("llms response"),
    )
    .await;
    assert_eq!(llms.status, StatusCode::OK);
    assert!(
        llms.headers["content-type"]
            .to_str()
            .expect("content type")
            .starts_with("text/plain")
    );
    let discovery = wire(
        "HTTP anonymous discovery after restart",
        client
            .get(format!("{}/api/v1/knowledgebases", server.http_url))
            .send()
            .await
            .expect("discovery"),
    )
    .await;
    assert_eq!(
        discovery.json()["knowledgebases"],
        serde_json::json!([PUBLIC_KB])
    );
    let browse = wire(
        "HTTP anonymous browse",
        client
            .get(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}",
                server.http_url
            ))
            .send()
            .await
            .expect("browse"),
    )
    .await;
    assert_eq!(browse.status, StatusCode::OK);
    assert!(browse.text().contains("public.md"));
    assert!(!browse.text().contains(".notedthat"));

    let range = wire(
        "HTTP anonymous range",
        client
            .get(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/public.md",
                server.http_url
            ))
            .header("Range", "bytes=0-5")
            .send()
            .await
            .expect("range"),
    )
    .await;
    assert_eq!(range.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.text(), "# Publ");
    assert!(range.headers.contains_key("content-range"));
    let head = wire(
        "HTTP anonymous HEAD",
        client
            .head(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/public.md",
                server.http_url
            ))
            .send()
            .await
            .expect("HEAD"),
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());

    let anonymous_search = search(client, server, "phase-three", None).await;
    assert_eq!(anonymous_search.status, StatusCode::OK);
    assert!(anonymous_search.text().contains("public.md"));
    assert!(!anonymous_search.text().contains(".notedthat"));
    let authenticated_search = search(client, server, INTERNAL_BODY, Some(API_TOKEN)).await;
    assert!(authenticated_search.text().contains(".notedthat/leak.md"));
}

async fn verify_credentials_and_writes(client: &reqwest::Client, server: &ServerInstance) {
    for authorization in ["Bearer wrong", "Basic Zm9vOmJhcg==", "Bearer "] {
        let invalid = wire(
            "HTTP supplied invalid credential",
            client
                .get(format!(
                    "{}/api/v1/knowledgebases/{PUBLIC_KB}/public.md",
                    server.http_url
                ))
                .header("Authorization", authorization)
                .send()
                .await
                .expect("invalid HTTP credential"),
        )
        .await;
        assert_http_401(&invalid);
    }
    let duplicate = wire(
        "HTTP duplicate Authorization smuggling probe",
        client
            .get(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/public.md",
                server.http_url
            ))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Authorization", "Bearer injected")
            .send()
            .await
            .expect("duplicate authorization"),
    )
    .await;
    assert_http_401(&duplicate);

    for path in [
        format!(
            "{}/api/v1/knowledgebases/{PRIVATE_KB}/private.md",
            server.http_url
        ),
        format!(
            "{}/api/v1/knowledgebases/{PUBLIC_KB}/.notedthat/manifest.json",
            server.http_url
        ),
    ] {
        assert_http_401(
            &wire(
                "HTTP hidden/private",
                client.get(path).send().await.expect("denied"),
            )
            .await,
        );
    }
    let private = wire(
        "HTTP authenticated private content",
        client
            .get(format!(
                "{}/api/v1/knowledgebases/{PRIVATE_KB}/private.md",
                server.http_url
            ))
            .bearer_auth(API_TOKEN)
            .send()
            .await
            .expect("private HTTP"),
    )
    .await;
    assert_eq!(private.text(), PRIVATE_BODY);

    for write_method in ["PUT", "PATCH", "POST", "DELETE"] {
        let denied = wire(
            &format!("HTTP anonymous {write_method}"),
            client
                .request(
                    method(write_method),
                    format!(
                        "{}/api/v1/knowledgebases/{PUBLIC_KB}/public.md",
                        server.http_url
                    ),
                )
                .body("attacker overwrite")
                .send()
                .await
                .expect("denied HTTP write"),
        )
        .await;
        assert_http_401(&denied);
    }
    let unchanged = wire(
        "HTTP authenticated unchanged after denied writes",
        client
            .get(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/public.md",
                server.http_url
            ))
            .bearer_auth(API_TOKEN)
            .send()
            .await
            .expect("unchanged HTTP"),
    )
    .await;
    assert_eq!(unchanged.text(), PUBLIC_BODY);
}
