mod handle;
mod wasi_runtime;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kube::client::APIClient;
use kubelet::pod::Pod;
use kubelet::{FileModuleStore, ModuleStore, Provider, ProviderError};
use log::{debug, info};
use tokio::fs::File;
use tokio::sync::RwLock;

use handle::PodHandle;
use wasi_runtime::WasiRuntime;

const TARGET_WASM32_WASI: &str = "wasm32-wasi";
const LOG_DIR_NAME: &str = "wasi-logs";

/// WasiProvider provides a Kubelet runtime implementation that executes WASM
/// binaries conforming to the WASI spec
#[derive(Clone)]
pub struct WasiProvider<S> {
    handles: Arc<RwLock<HashMap<String, PodHandle<File>>>>,
    store: S,
    log_path: PathBuf,
}

impl<C> WasiProvider<FileModuleStore<C>> {
    /// Returns a new WASI provider configured to use the proper data directory
    /// (including creating it if necessary)
    pub async fn new<P: AsRef<Path>>(client: C, data_dir: P) -> anyhow::Result<Self> {
        // Make sure we have a log dir and containers dir created
        let data_path = data_dir.as_ref().to_path_buf();
        // This is temporary as we should probably be passing in a ModuleStore
        // as a parameter or the oci client as a whole
        // NOTE: We do not have to create the dir here as the FileModuleStore already does this
        let mut container_path = data_path.join("containers");
        container_path.push(".oci");
        container_path.push("modules");
        let log_path = data_path.join(LOG_DIR_NAME);
        tokio::fs::create_dir_all(&log_path).await?;
        let store = FileModuleStore::new(client, &container_path);
        Ok(Self {
            handles: Default::default(),
            store,
            log_path,
        })
    }
}

#[async_trait::async_trait]
impl<S: ModuleStore + Send + Sync> Provider for WasiProvider<S> {
    async fn init(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn arch(&self) -> String {
        TARGET_WASM32_WASI.to_string()
    }

    fn can_schedule(&self, pod: &Pod) -> bool {
        // If there is a node selector and it has arch set to wasm32-wasi, we can
        // schedule it.
        match pod.node_selector() {
            Some(node_selector) => node_selector
                .get("beta.kubernetes.io/arch")
                .map(|v| v == TARGET_WASM32_WASI)
                .unwrap_or(false),
            _ => false,
        }
    }

    async fn add(&self, pod: Pod, client: APIClient) -> anyhow::Result<()> {
        // To run an Add event, we load the WASM, update the pod status to Running,
        // and then execute the WASM, passing in the relevant data.
        // When the pod finishes, we update the status to Succeeded unless it
        // produces an error, in which case we mark it Failed.

        // TODO: Implement this for real.
        //
        // What it should do:
        // - for each volume
        //   - set up the volume map
        // - for each init container:
        //   - set up the runtime
        //   - mount any volumes (preopen)
        //   - run it to completion
        //   - bail with an error if it fails
        // - for each container and ephemeral_container
        //   - set up the runtime
        //   - mount any volumes (popen)
        //   - run it to completion
        //   - bail if it errors
        let pod_name = pod.name().to_owned();
        info!("Starting containers for pod {:?}", pod_name);
        // Wrap this in a block so the write lock goes out of scope when we are done
        let mut container_handles = HashMap::new();

        let mut modules = self.store.fetch_container_modules(&pod).await?;

        for container in pod.containers() {
            let env = self.env_vars(client.clone(), &container, &pod).await;
            let module_data = modules
                .remove(&container.name)
                .expect("FATAL ERROR: module map not properly populated");

            let runtime = WasiRuntime::new(
                module_data,
                env,
                Vec::default(),
                HashMap::default(),
                self.log_path.clone(),
            )
            .await?;

            debug!("Starting container {} on thread", container.name);
            let handle = runtime.start().await?;
            container_handles.insert(container.name.clone(), handle);
        }
        {
            // Grab the entry while we are creating things
            let mut handles = self.handles.write().await;
            handles.insert(
                key_from_pod(&pod),
                PodHandle::new(container_handles, pod, client)?,
            );
        }
        info!(
            "All containers started for pod {:?}. Updating status",
            pod_name
        );
        Ok(())
    }

    async fn modify(&self, pod: Pod, _client: APIClient) -> anyhow::Result<()> {
        // Modify will be tricky. Not only do we need to handle legitimate modifications, but we
        // need to sift out modifications that simply alter the status. For the time being, we
        // just ignore them, which is the wrong thing to do... except that it demos better than
        // other wrong things.
        info!("Pod modified");
        info!(
            "Modified pod spec: {:#?}",
            pod.as_kube_pod().status.as_ref().unwrap()
        );
        Ok(())
    }

    async fn delete(&self, pod: Pod, _client: APIClient) -> anyhow::Result<()> {
        // There is currently no way to stop a long running instance, so we are
        // SOL here until there is support for it. See
        // https://github.com/bytecodealliance/wasmtime/issues/860 for more
        // information. For now, just delete the handle from the map
        let mut handles = self.handles.write().await;
        handles.remove(&key_from_pod(&pod));
        unimplemented!("cannot stop a running wasmtime instance")
    }

    async fn logs(
        &self,
        namespace: String,
        pod_name: String,
        container_name: String,
    ) -> anyhow::Result<Vec<u8>> {
        let mut handles = self.handles.write().await;
        let handle = handles
            .get_mut(&pod_key(&namespace, &pod_name))
            .ok_or_else(|| ProviderError::PodNotFound {
                pod_name: pod_name.clone(),
            })?;
        let mut output = Vec::new();
        handle.output(&container_name, &mut output).await?;
        Ok(output)
    }
}

/// Generates a unique human readable key for storing a handle to a pod
fn key_from_pod(pod: &Pod) -> String {
    pod_key(pod.namespace(), pod.name())
}

fn pod_key<N: AsRef<str>, T: AsRef<str>>(namespace: N, pod_name: T) -> String {
    format!("{}:{}", namespace.as_ref(), pod_name.as_ref())
}

#[cfg(test)]
mod test {
    use super::*;
    use k8s_openapi::api::core::v1::Pod as KubePod;
    use k8s_openapi::api::core::v1::PodSpec;

    #[tokio::test]
    async fn test_can_schedule() {
        // TODO: stop using actual client in unit tests.
        let client = oci_distribution::Client::default();
        let wp = WasiProvider::new(client, "./foo")
            .await
            .expect("unable to create new runtime");
        let mock = Default::default();
        assert!(!wp.can_schedule(&mock));

        let mut selector = std::collections::BTreeMap::new();
        selector.insert(
            "beta.kubernetes.io/arch".to_string(),
            "wasm32-wasi".to_string(),
        );
        let mut mock: KubePod = mock.into();
        mock.spec = Some(PodSpec {
            node_selector: Some(selector.clone()),
            ..Default::default()
        });
        let mock = Pod::new(mock);
        assert!(wp.can_schedule(&mock));
        selector.insert("beta.kubernetes.io/arch".to_string(), "amd64".to_string());
        let mut mock: KubePod = mock.into();
        mock.spec = Some(PodSpec {
            node_selector: Some(selector),
            ..Default::default()
        });
        let mock = Pod::new(mock);
        assert!(!wp.can_schedule(&mock));
    }
}
