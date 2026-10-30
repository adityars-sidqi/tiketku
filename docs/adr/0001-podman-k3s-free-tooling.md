# ADR 0001: Tool gratis, Podman sebagai container runtime, k3s sebagai cluster lokal

- Status: Diterima
- Tanggal: 31 Oktober 2026
- Story terkait: #2 (docker-compose), #50 sampai #55 (container, Kubernetes, CI/CD)

## Konteks

- TiketKu adalah project belajar. User ingin semua tool **gratis penuh**: open source, tanpa tier berbayar, dan tanpa lisensi yang membatasi pemakaian.
- Pekerjaan sehari-hari berjalan di WSL2 (Ubuntu 24.04, systemd aktif). Docker belum terpasang.
- `docs/SOAL.md` sebelumnya menyebut `docker compose` untuk lingkungan lokal dan kind atau k3d untuk cluster lokal.
- Service harus bisa di-deploy ke Kubernetes mana pun, tidak terkunci pada satu tool lokal atau satu cloud.

## Opsi

### Container runtime

| Opsi | Lisensi | Kelebihan | Kekurangan |
| --- | --- | --- | --- |
| Docker Desktop | Proprietary, berbayar untuk perusahaan besar | GUI, integrasi WSL otomatis | Tidak gratis penuh, berat |
| Docker Engine (CE) di WSL | Apache 2.0 | Ekosistem paling luas | Daemon root yang selalu jalan |
| **Podman** di WSL | Apache 2.0 | Tanpa daemon, rootless, kompatibel dengan CLI Docker dan format Compose | `podman compose` butuh provider; beberapa tool perlu `DOCKER_HOST` diarahkan ke socket Podman |

### Cluster Kubernetes lokal

| Opsi | Lisensi | Kelebihan | Kekurangan |
| --- | --- | --- | --- |
| kind + Podman | Apache 2.0 | Cepat dibuat ulang, populer di CI | Provider Podman masih bertanda experimental |
| **k3s** di WSL | Apache 2.0 | Distribusi Kubernetes tersertifikasi CNCF, ringan, sudah termasuk metrics-server, local-path storage, dan load balancer sederhana; paling mirip cluster sungguhan | Terpasang sebagai service sistem; punya containerd sendiri, jadi image dari Podman harus di-import |
| minikube + Podman | Apache 2.0 | Banyak addon | Driver Podman experimental, lebih berat |

## Keputusan

1. **Hanya memakai tool gratis dan open source** (lisensi OSI). Saat memilih tool baru, lisensinya dicek dulu dan disebutkan di rekomendasi. Contoh: OpenTofu, bukan Terraform.
   - **Pengecualian: Redis.** Redis sejak 7.4 berlisensi RSALv2/SSPL (Redis 8 menambah opsi AGPLv3). Lisensi itu gratis untuk dipakai sebagai cache di project ini, jadi Redis tetap dipakai atas keputusan user. Valkey (BSD) tetap menjadi jalur keluar bila suatu saat dibutuhkan, karena kompatibel di level protokol.
2. **Podman** menjadi container runtime lokal. File Compose tetap memakai format Compose Spec (`deploy/docker-compose.yml`) dan dijalankan dengan `podman compose`. Provider-nya binary `docker-compose` v2 (Apache 2.0) yang terhubung ke socket Podman, supaya `profiles`, `--wait`, dan `depends_on: condition: service_healthy` berjalan.
3. Image ditulis sebagai **OCI** lewat `Dockerfile` (Podman dan Buildah membacanya tanpa perubahan) dan dibangun dengan `podman build`.
4. **k3s** menjadi cluster lokal, dipasang dengan `--disable traefik` karena ingress memakai APISIX. Context kubeconfig dinamai `k3s-tiketku`. Image lokal dimasukkan dengan `podman save <image> | sudo k3s ctr images import -`.
5. **Manifest harus portabel**: hanya API Kubernetes standar dan Kustomize. `base/` tidak boleh berisi resource khusus distro atau cloud; hal yang spesifik target (storage class, tipe Service, host ingress) diletakkan di overlay. Manifest harus bisa jalan di k3s, kind, minikube, cluster managed (EKS, GKE, AKS), maupun OpenShift: container non-root, aman dijalankan dengan UID acak, tidak memakai port privileged, dan `readOnlyRootFilesystem`.

## Konsekuensi

- Perintah di dokumentasi dan definisi agent memakai `podman` dan `podman compose`, bukan `docker`.
- Testcontainers (Rust `testcontainers`, Playwright, dan sejenisnya) diarahkan ke Podman lewat `DOCKER_HOST=unix:///run/user/$UID/podman/podman.sock`, dengan `podman.socket` user aktif. Jika Ryuk bermasalah pada mode rootless, set `TESTCONTAINERS_RYUK_DISABLED=true`.
- Healthcheck container di Podman dijalankan lewat timer systemd. Ini aman karena systemd aktif di WSL.
- Cluster lokal berganti dari kind/k3d ke k3s. Pemeriksaan keamanan sebelum `kubectl apply` memakai context lokal `k3s-tiketku` (atau `kind-*` dan `k3d-*` bila dipakai di CI).
- CI di GitHub Actions boleh memakai Docker bawaan runner atau kind untuk uji manifest. Hasilnya tetap image OCI dan manifest yang sama.
- Pemasangan awal (perlu `sudo`, dijalankan user):

  ```bash
  sudo apt install -y podman
  systemctl --user enable --now podman.socket
  # Provider compose: binary docker-compose v2 (rilis resmi GitHub)
  # k3s, saat fase F8 atau saat cluster mulai dibutuhkan
  curl -sfL https://get.k3s.io | sh -s - --disable traefik --write-kubeconfig-mode 644
  ```
