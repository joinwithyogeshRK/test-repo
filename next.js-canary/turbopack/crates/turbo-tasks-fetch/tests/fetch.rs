#![allow(clippy::needless_return)] // tokio macro-generated code doesn't respect this
#![cfg(test)]

use tokio::sync::Mutex as TokioMutex;
use turbo_rcstr::{RcStr, rcstr};
use turbo_tasks::Vc;
use turbo_tasks_fetch::{
    __test_only_reqwest_client_cache_clear, __test_only_reqwest_client_cache_len, FetchClient,
    FetchErrorKind,
};
use turbo_tasks_fs::{DiskFileSystem, FileSystem, FileSystemPath};
use turbo_tasks_testing::{Registration, register, run};
use turbopack_core::issue::{Issue, IssueSeverity, StyledString};

static REGISTRATION: Registration = register!(turbo_tasks_fetch::register);

/// We inspect information about the global client cache, so *every* test in this process *must*
/// acquire and hold this lock to prevent potential flakiness.
static GLOBAL_TEST_LOCK: TokioMutex<()> = TokioMutex::const_new(());

#[tokio::test]
async fn basic_get() {
    let _guard = GLOBAL_TEST_LOCK.lock().await;
    run(&REGISTRATION, || async {
        let mut server = mockito::Server::new_async().await;
        let resource_mock = server
            .mock("GET", "/foo.woff")
            .with_body("responsebody")
            .create_async()
            .await;

        let client_vc = FetchClient::default().cell();
        let response = &*client_vc
            .fetch(
                RcStr::from(format!("{}/foo.woff", server.url())),
                /* user_agent */ None,
            )
            .await?
            .unwrap()
            .await?;

        resource_mock.assert_async().await;

        assert_eq!(response.status, 200);
        assert_eq!(*response.body.to_string().await?, "responsebody");
        anyhow::Ok(())
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn sends_user_agent() {
    let _guard = GLOBAL_TEST_LOCK.lock().await;
    run(&REGISTRATION, || async {
        let mut server = mockito::Server::new_async().await;
        let resource_mock = server
            .mock("GET", "/foo.woff")
            .match_header("User-Agent", "mock-user-agent")
            .with_body("responsebody")
            .create_async()
            .await;

        eprintln!("{}", server.url());

        let client_vc = FetchClient::default().cell();
        let response = &*client_vc
            .fetch(
                RcStr::from(format!("{}/foo.woff", server.url())),
                Some(rcstr!("mock-user-agent")),
            )
            .await?
            .unwrap()
            .await?;

        resource_mock.assert_async().await;

        assert_eq!(response.status, 200);
        assert_eq!(*response.body.to_string().await?, "responsebody");
        anyhow::Ok(())
    })
    .await
    .unwrap()
}

// This is temporary behavior.
// TODO: Implement invalidation that respects Cache-Control headers.
#[tokio::test]
async fn invalidation_does_not_invalidate() {
    let _guard = GLOBAL_TEST_LOCK.lock().await;
    run(&REGISTRATION, || async {
        let mut server = mockito::Server::new_async().await;
        let resource_mock = server
            .mock("GET", "/foo.woff")
            .with_body("responsebody")
            .with_header("Cache-Control", "no-store")
            .create_async()
            .await;

        let url = RcStr::from(format!("{}/foo.woff", server.url()));
        let client_vc = FetchClient::default().cell();
        let response = &*client_vc
            .fetch(url.clone(), /* user_agent */ None)
            .await?
            .unwrap()
            .await?;

        resource_mock.assert_async().await;

        assert_eq!(response.status, 200);
        assert_eq!(*response.body.to_string().await?, "responsebody");

        let second_response = &*client_vc
            .fetch(url.clone(), /* user_agent */ None)
            .await?
            .unwrap()
            .await?;

        // Assert that a second request is never sent -- the result is cached via turbo tasks
        resource_mock.expect(1).assert_async().await;

        assert_eq!(response, second_response);
        anyhow::Ok(())
    })
    .await
    .unwrap()
}

fn get_issue_context() -> Vc<FileSystemPath> {
    DiskFileSystem::new(rcstr!("root"), rcstr!("/")).root()
}

#[tokio::test]
async fn errors_on_failed_connection() {
    let _guard = GLOBAL_TEST_LOCK.lock().await;
    run(&REGISTRATION, || async {
        // Try to connect to port 0 on localhost, which is never valid and immediately returns
        // `ECONNREFUSED`.
        // Other values (e.g. domain name, reserved IP address block) may result in long timeouts.
        let url = rcstr!("http://127.0.0.1:0/foo.woff");
        let client_vc = FetchClient::default().cell();
        let response_vc = client_vc.fetch(url.clone(), None);
        let err_vc = &*response_vc.await?.unwrap_err();
        let err = err_vc.await?;

        assert_eq!(*err.kind.await?, FetchErrorKind::Connect);
        assert_eq!(*err.url.await?, url);

        let issue = err_vc.to_issue(IssueSeverity::Error, get_issue_context().owned().await?);
        assert_eq!(issue.await?.severity(), IssueSeverity::Error);
        assert_eq!(
            *issue.description().await?.unwrap().await?,
            StyledString::Text(rcstr!(
                "There was an issue establishing a connection while requesting \
                http://127.0.0.1:0/foo.woff."
            ))
        );
        anyhow::Ok(())
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn errors_on_404() {
    let _guard = GLOBAL_TEST_LOCK.lock().await;
    run(&REGISTRATION, || async {
        let mut server = mockito::Server::new_async().await;
        let resource_mock = server
            .mock("GET", "/")
            .with_status(404)
            .create_async()
            .await;

        let url = RcStr::from(server.url());
        let client_vc = FetchClient::default().cell();
        let response_vc = client_vc.fetch(url.clone(), None);
        let err_vc = &*response_vc.await?.unwrap_err();
        let err = err_vc.await?;

        resource_mock.assert_async().await;
        assert!(matches!(*err.kind.await?, FetchErrorKind::Status(404)));
        assert_eq!(*err.url.await?, url);

        let issue = err_vc.to_issue(IssueSeverity::Error, get_issue_context().owned().await?);
        assert_eq!(issue.await?.severity(), IssueSeverity::Error);
        assert_eq!(
            *issue.description().await?.unwrap().await?,
            StyledString::Text(RcStr::from(format!(
                "Received response with status 404 when requesting {url}"
            )))
        );
        anyhow::Ok(())
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn client_cache() {
    // a simple fetch that should always succeed
    async fn simple_fetch(path: &str, client: FetchClient) -> anyhow::Result<()> {
        let mut server = mockito::Server::new_async().await;
        let _resource_mock = server
            .mock("GET", &*format!("/{path}"))
            .with_body("responsebody")
            .create_async()
            .await;

        let url = RcStr::from(format!("{}/{}", server.url(), path));
        let response = match &*client
            .cell()
            .fetch(url.clone(), /* user_agent */ None)
            .await?
        {
            Ok(resp) => resp.await?,
            Err(_err) => {
                anyhow::bail!("fetch error")
            }
        };

        if response.status != 200 {
            anyhow::bail!("non-200 status code")
        }

        anyhow::Ok(())
    }

    let _guard = GLOBAL_TEST_LOCK.lock().await;
    run(&REGISTRATION, || async {
        __test_only_reqwest_client_cache_clear();
        assert_eq!(__test_only_reqwest_client_cache_len(), 0);

        simple_fetch(
            "/foo",
            FetchClient {
                tls_built_in_native_certs: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(__test_only_reqwest_client_cache_len(), 1);

        // the client is reused if the config is the same (by equality)
        simple_fetch(
            "/bar",
            FetchClient {
                tls_built_in_native_certs: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(__test_only_reqwest_client_cache_len(), 1);

        // the client is recreated if the config is different
        simple_fetch(
            "/bar",
            FetchClient {
                tls_built_in_native_certs: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(__test_only_reqwest_client_cache_len(), 2);

        Ok(())
    })
    .await
    .unwrap()
}
