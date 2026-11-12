# 静态发行包的公共 TLS 根证书

`mozilla-ca-bundle.pem` 来自 [curl 官方 CA Extract](https://curl.se/docs/caextract.html)，
下载于 2026-11-12。证书数据日期为 2026-11-05，共 118 个证书。

SHA256：`427c064216761a1e72a8b30f0d954d08cb6e4435a82091fb865afd20c8497285`，
已与 curl 官方 `cacert.pem.sha256` 核对。

证书包遵循 Mozilla Public License 2.0，保留原始 PEM 中的来源与许可说明。
源文件：https://curl.se/ca/cacert.pem 。

仅 `standalone-tls` 构建将此文件编译到程序中，发行 ZIP 不附带独立证书文件。
更新证书时需重新校验来源、完整性和 TLS 行为，再重新构建发行程序。
