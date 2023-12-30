# overlayfs-csi

This implements a Kubernetes [Container Storage Interface](https://github.com/container-storage-interface/spec/blob/master/spec.md) that creates volumes as [overlay mounts](https://en.wikipedia.org/wiki/OverlayFS) on top of previous volumes. In other words, only new and modified files are written to disk.

This can be particularly useful in build pipelines, as it allows benefiting from incremental compilation (whenever supported) without having to copy all files for each run.

This is similar to using `dataSource` in CSIs that support efficient [volume cloning](https://kubernetes.io/docs/concepts/storage/volume-pvc-datasource/), except that maintaining base volumes is handled by the CSI automatically.

This repository also provides an example for building Kubernetes CSIs in Rust.

## Usage

- Each pod requests a data volume from the CSI:

  ```yaml
  volumes:
    - name: data
      csi:
        driver: overlayfs.csi.k8s.io
  ```

- By writing a `.as_base` file on the volume, a pod can indicate that the volume can later be used as a _base_ for subsequent volumes.

  - TODO: This could be replaced by a check on the pod exit status.

- Whenever a base is available, the volume provided by the CSI is an overlay filesystem on top of it. Otherwise, it starts empty.

- Old bases are cleaned up with a configurable interval. When this results in no base being available, the next vollume will be created from scratch, and then converted to a base.
  - Converting overlays into bases is currently not supported.

### Underlying storage

For now, only node-local storage is supported for the bases (which in particular allows quickly converting volumes to bases) and overlays. In particular, this implies that each node maintains its own bases.

It would be fairly easy to support arbitrary volumes type. For CSIs that support efficient [volume cloning](https://kubernetes.io/docs/concepts/storage/volume-pvc-datasource/), these could be used instead of the overlays.

## Installation

> [!CAUTION]
> This is for now completely experimental, use at your own risk.

1. Compile the binary and build the docker image:

   ```
   $ cd docker
   $ cross build -r --target-dir ../target-cross
   $ cp ../target-cross/release/csi .
   $ docker build -t overlayfs-csi .
   ```

2. Customize values in the [Helm chart](https://helm.sh/) (`chart/values.yaml`)
3. Apply the chart
   ```
   $ helm install overlayfs-csi chart
   ```

The test the deployment:

1. Create a pod
   ```yaml
   apiVersion: v1
   kind: Pod
   metadata:
     generateName: test
   spec:
     nodeName: node1
     terminationGracePeriodSeconds: 1
     volumes:
       - name: test
         csi:
           driver: overlayfs.csi.k8s.io
     containers:
       - name: test
         image: debian:bullseye-slim
         command: ["sleep", "infinity"]
         volumeMounts:
           - name: test
             mountPath: /test
   ```
2. Write some data and `.as_base` to the volume
   ```
   $ touch /test/.as_base
   $ touch /test/hello
   ```
3. Delete the pod
4. Create a new pod, and observe that it now used the previous volume as base.
   ```
   $ ls /test
   hello
   $ touch /test/hi
   ```
   Data written to that pod goes exclusively to the overlay. The base is not modified, and no untouched file is copied.

## Implementation details

- A single Rust binary implements the required Identity and Node CSI services. Kubelet communicates with it using a UNIX socket.
- A daemonset runs one such server per node, following the Kubernetes CSI design.
- Each server has a `bases` volume, where bases are kept.
- When the server receives a volume publishing request, either:
  - There are no bases available and a bind mount is made with an empty folder.
  - There is a a base available, and an overlayfs mount is made.
- When the server receives a volume unpublishing request, if there are no bases available and the volume is a candidate, it converts the volume into a base. Otherwise, the volume is simply removed. Only volumes created from scratch, not overlays, can be converted into bases.
- The server cleans stale bases regularly.
- To be able to properly interact with [ephemeral storage limits](https://kubernetes.io/docs/concepts/configuration/manage-resources-containers/#local-ephemeral-storage) (and later with other underlying storages), the overlay upper and work layers (where new and modified files are written) are taken from dynamically scheduled pods. This is required, as we cannot dynamically attach new volumes to the CSI pods.
- When moving from a pod to the `base` volume, we have to access the volume from the host path (`/var/lib/kubelet/pods/{}/volumes/`) to avoid spurious cross-device errors.

## TODOs

- Deduce whether a volume can be used as base from the pod status (see above).
- Support other underlying storages (see above).
- Allow different categories of bases with labels.
