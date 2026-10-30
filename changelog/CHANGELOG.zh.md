# 更新日志

## [1.3.0] - 2026-10-30

### 新增
- **SSH 隧道配置文件：** 在设置中将 SSH 隧道（服务器、跳板机、用户和身份验证）保存为配置文件，只需一次，即可在任何连接中选用；每个连接仍保留自己的数据库服务器。编辑配置文件后，所有使用它的连接会在下次连接时生效；连接当前的隧道可以保存为配置文件，配置文件的密钥仍保存在系统钥匙串中。
- **悬停显示表的列：** 在 SQL 编辑器中将指针停在表名上，会显示该表的列，包括类型、是否允许为空、主键以及每列的注释，适用于所有引擎。

### 改进
- **使用 Claude Code 和 Codex 的 AI 助手：** 当您询问同一连接中的其他数据库时，助手会读取其表和列，而不会切换标签页的数据库。这些模型依然不会收到表中的行数据。

### 修复
- **Linux 上的 SAP HANA，安全：** 取消查询时，不再在会话删除受信任证书文件夹之后读取该文件夹；此前，同一台计算机上的其他用户可能趁这一刻植入自己的证书颁发机构。
- **安全，依赖项：** `ctutils` 和 `crypto-bigint` 已更新到包含恒定时间修复的版本之后。
- **超长文本的 Excel 导出：** 当单元格内容超过 Excel 的限制且包含表情符号等字符时，会再次被截断以适应限制，而不是导致导出中止。
- **Trino 和 Presto，安全：** 从列类型嵌套深度超过任何真实架构的表中复制或导出数据，不会再导致 DBine 崩溃；1.2.2 中加入的深度限制现在也覆盖了这条路径。

感谢 [@dylanwu](https://github.com/dylanwu)，他的详细反馈促成了此版本中的三项新功能。

## [1.2.2] - 2026-10-28

### 修复
- **安全：已修复 ZIP 处理中的严重漏洞。** 10 月 28 日，DBine 用于读写 ZIP 类文件（Excel 导入和导出，以及 AI 助手内置模型的下载）的库 `zip` 公布了一个严重漏洞（[GHSA-m665-v7pm-7grj](https://github.com/advisories/GHSA-m665-v7pm-7grj)）。特制的压缩包可能会因读取方式不同而显示不同的内容。此版本已将 DBine 的所有部分迁移到修复后的库；它仅因此原因在 1.2.1 发布数小时后推出，建议更新。
- **Trino 和 Presto，安全：** 嵌套深度超出任何真实架构的列类型（恶意服务器可能发送此类类型）现在会按原样显示，而不是逐层解析，因此不会再导致 DBine 崩溃。

## [1.2.1] - 2026-10-28

### 修复
- **列补全跟随你正在编写的查询:** 编辑器只提供光标所在语句读取的表、CTE 和子查询的列，而不是文件中其他语句的列。两张表共有的列会按表各提供一次，以别名作为标签，并以限定形式插入（`c.customer_id`）；CTE 或派生表的列会在查询内部以及其别名之后提供。
- **行和 struct 中的字段名:** Trino 和 Presto 的 `ROW` 值、PostgreSQL 的复合类型以及 Arrow Flight SQL 的 struct 以 JSON 形式按名称显示每个字段，嵌套行、行数组以及值为行的映射中也是如此（`[{"customer_id":42,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]`，而不是 `[[42,[["demo-product",2]],null]]`）。`null`、空字符串和空数组仍可区分。ClickHouse 的命名元组保持声明的字段顺序。
- **使用没有 VIEW DEFINITION 权限的 SQL Server 登录进行迁移:** 当源登录可以读取表但无法读取其定义时（在 Azure SQL 上很常见），迁移脚本会写出 `CHECK ()`，导致 "Incorrect syntax near ')'" 错误，并且会在不提示的情况下丢弃列默认值。现在这些 CHECK、默认值和计算表达式会从脚本中省略，预览和运行报告会在顶部逐一列出，并给出原因以及让 DBine 读取它们的 `GRANT VIEW DEFINITION`。生成的脚本和架构同步也会给出提示，同步不再因为源无法读取自身的 CHECK 而删除目标上的 CHECK。所有引擎都不再写出空的 CHECK 或 DEFAULT。

## [1.2.0] - 2026-10-27

### 新增
- **乌克兰语和波兰语：** DBine 和手机应用现已提供 Українська 和 Polski，包括服务器消息和 AI 助手。在 **设置 › 常规 › 语言** 中选择语言，或让 DBine 跟随系统语言。
- **印度尼西亚语：** DBine 和手机应用现已提供 Bahasa Indonesia，包括服务器消息和 AI 助手。在 **设置 › 常规 › 语言** 中选择语言，或让 DBine 跟随系统语言。

### 改进
- **跨数据库补全：** 对于一个查询可以读取同一连接中其他数据库的引擎（ClickHouse、MySQL 和 MariaDB、SQL Server、Snowflake、Databricks、Trino、BigQuery、Oracle 等），编辑器会提示其他数据库的名称、`other_db.` 之后的表，以及表或别名之后的列，跨数据库的联接中同样适用。每个数据库的表在你第一次输入其名称并加点号时读取。
- **DuckDB 引擎更新到 2.0.0：** 本地 DuckDB 文件和笔记本中的 DataFrame SQL 现在运行在 DuckDB 2.0.0 上（此前为 1.5.5）。新的数据库文件使用 DuckDB 2.0 的存储格式，旧版 DuckDB 无法打开；已有文件仍可正常打开，并且创建它们的版本仍可读取。
- **SQL Server、PostgreSQL 和 MySQL 的连接加密：** 连接表单显示各引擎自己的 TLS 选项，名称与其工具所用的一致：SQL Server 和 Azure SQL 为 `Encrypt`（Optional、Mandatory、Strict）、`TrustServerCertificate`、`ServerCertificate` 和 `HostNameInCertificate`；PostgreSQL 及其系列为 `sslmode` 和 `sslrootcert`；MySQL 及其系列为 `ssl-mode` 和 `ssl-ca`；MariaDB 和 OceanBase 为 `ssl`、`ssl-verify-server-cert` 和 `ssl-ca`。新连接默认加密并校验服务器证书，每项设置都可以关闭。已保存的连接保持原有行为不变。通过 SSH 隧道时，证书会按真实服务器的名称校验，证书失败时会提示应更改哪个选项。MySQL 的 `PREFERRED` 仅在服务器不提供 TLS 时才回退到未加密连接，且发生在发送任何密码之前。
- **InfluxDB、IoTDB、ksqlDB、TDengine、Phoenix、Arrow Flight SQL 和云服务的连接加密：** 连接表单显示各引擎自己的 TLS 选项：InfluxDB 1 为 `ssl` 和 `unsafeSsl`；InfluxDB 2 为带 `skip-verify` 的 URL 协议，InfluxDB 3 为 `tls-no-verify` 和 `tls-ca`；IoTDB 和 TimechoDB 为 `use_ssl` 和 `ca_certs`；ksqlDB 为 `useTls`、`verifyHost` 和 `trustStore`；TDengine 为 `useSSL`；Phoenix 和 Avatica 为 URL 协议、`hostname_verification` 和 `truststore`；Flight SQL 为 `useEncryption` 和 `tlsRootCerts`；Cosmos DB 模拟器为 `DisableServerCertificateValidation`；当引擎的工具没有跳过校验或指定 CA 文件的名称时，使用 curl 的 `insecure` 和 `cacert`。新连接默认加密并校验服务器证书，每项设置都可以关闭。已保存的连接保持原有行为不变。通过 SSH 隧道时，证书会按真实服务器的名称校验，证书失败或服务器不支持 TLS 时会如实报告。BigQuery、Spanner、DynamoDB、Athena、Snowflake、Databricks 和 libSQL 始终使用经过校验的 HTTPS，但以 `http://` 输入的模拟器端点除外。
- **键盘和屏幕阅读器：** 可见的焦点环显示键盘当前位置；列表行、卡片、可排序的表头和可折叠的区域可以用 Tab 到达，并用 Enter 或空格操作；上下文菜单可用方向键操作；仅有图标的按钮带有屏幕阅读器可朗读的名称；查询的结果（完成，或失败及其错误）会被播报；系统要求减少动态效果时动画会停止；两种主题下次要文字的对比度都更高。

### 修复
- **“不验证”现在会固定服务器证书，安全:** 不验证服务器证书的连接（`sslmode=require`, `insecure`, `bolt+ssc`, `validate=false`…）会接受服务器首次出示的证书，此后只接受该证书，适用于 PostgreSQL 及其同系列、HTTP 引擎（ClickHouse、Elasticsearch 和 OpenSearch、CouchDB、Trino、InfluxDB 等）、Neo4j、Neptune、Redis 和 Cassandra。如果证书发生变化，连接会中断并显示两个指纹；对于已更新证书的服务器，可使用**信任新证书**；SSL/TLS 选项卡会显示已固定的指纹，并提供**忘记**。已保存的连接会在下次连接时固定证书。`verify-ca` 以及仅跳过主机名检查的选项现在会对照 CA 检查整条证书链，而不是接受任何证书。SQL Server、MySQL、MariaDB、MongoDB 和 SAP HANA 保持其客户端自身的行为。
- **新数据库无需重新连接即可显示：** 在查询中用 `CREATE DATABASE` 创建（或删除、重命名）的数据库会立即出现在资源管理器和数据库列表中，打开数据库列表（迁移、比较、subset、备份、权限、查询的数据库选择器等）时会重新读取，因此从其他工具创建的数据库也会显示。
- **从 SQL Server Management Studio 导入：** 除已注册的服务器外，DBine 现在还会导入 SSMS 连接对话框记住的服务器（其连接历史），包括登录名、数据库和加密设置，支持 SSMS 18 到 20 以及 SSMS 21 及更高版本的新文件夹。在 macOS 和 Linux 上，文件对话框现在接受 `RegSrvr.xml`、`UserSettings.xml` 和 `.regsrvr` 文件（以及 DataGrip 的 `dataSources.xml`）；此前只能选择 JSON 文件。
- **导入连接字符串时保留加密设置：** 粘贴或导入的字符串现在会带上各引擎自己的 TLS 选项：SQL Server 的 `Encrypt`（包括 `Strict`）和 `HostNameInCertificate`，PostgreSQL 的 `sslmode` 按原样保留（`require` 不再变成经过校验的连接），MySQL 的 `ssl-mode` 和 Connector/J 的 `useSSL`，MariaDB 的 `sslMode`，以及 `rediss://`、`bolt+s://` 和 `bolt+ssc://` 协议。不含 TLS 设置的字符串按其自身客户端的连接方式导入。从 DBeaver、DataGrip 及其他 JetBrains IDE、DbGate 和 Azure Data Studio 导入的连接同样适用。
- **macOS 和 Windows 上的证书错误：** 系统不信任的服务器证书（“The certificate was not trusted”“issued by an authority that is not trusted”“The target principal name is incorrect”）现在能被识别，消息会说明应更改哪个 TLS 选项，而不是显示笼统的 TLS 错误。
- **ClickHouse 22.8 及更早版本：** 连接不再因 `Unknown setting http_write_exception_in_output_format` 而失败；DBine 不再向没有该设置的服务器请求它。
- **笔记本，Python 导出：** 校验服务器证书的 PostgreSQL 连接现在导出为 `sslmode=verify-full`，与 **复制连接字符串** 一致，而不是不做校验的 `require`。
- **HashiCorp Vault，安全：** 密码来源的 Vault 地址必须是 `https://`；仅本机上的 Vault 才接受明文 `http://`，与其他密钥管理器一致，因此 Vault 令牌绝不会在网络上以明文传输。

## [1.1.0] - 2026-10-21

### 新增
- **云同步状态一目了然：** 状态栏显示带绿点的 **已同步**，更改等待上传时显示琥珀色的 **待上传**，出错时显示红色；悬停可查看上次同步时间，点击可打开同步设置。
- **复制连接字符串：** 右键单击连接（或使用其表单中的按钮），选择 **复制连接字符串…**，即可得到 URI、ADO.NET、JDBC、SQLAlchemy、ODBC、libpq、Node.js 或 Go 格式的字符串（以其引擎支持的为准），适用于任何数据库，并包含其 TLS 和登录设置。密码默认不包含（用 `${DB_PASSWORD}` 占位符代替），除非开启 **包含密码**；开启后会在复制时获取密码，并在一分钟后若仍在剪贴板中则将其清除。
- 计划任务新增 **发送消息** 步骤：可将运行结果发送到 Microsoft Teams、Slack、Telegram 或 ntfy，支持任务变量（`{results}` 列出每个步骤的执行情况）、与邮件步骤相同的 **仅当…** 条件，以及 **测试** 按钮。Webhook URL 和令牌保存在钥匙串中，不会存入任务，消息仅通过 HTTPS 发送到该服务自己的服务器。

### 修复
- **SSH 隧道，安全：** 更新 SSH 库，修复两个安全公告（CVE-2026-111061，通过 SSH 代理造成的拒绝服务，以及 CVE-2026-111062）。
- **RSA 密钥，安全：** 使用 RSA 密钥登录 SSH，以及打开 Zoho Vault 和 Bitwarden 的组织密钥，现在都在 AWS-LC 中以恒定时间运行，因此 RSA 库中的时序泄漏（RUSTSEC-2023-0071，“Marvin”）不再适用。RSA 密钥使用 SHA-256 或 SHA-512 签名；只接受 SHA-1（`ssh-rsa`）的服务器需要改用 Ed25519 或 ECDSA 密钥，或 SSH 代理。
- **TLS，安全：** DBine 和 33 个驱动中的 TLS 库及其证书校验已更新（RUSTSEC-2026-0346 至 0349 以及 0381 至 0383），因此来自不再受信任的证书颁发机构的证书会被拒绝。更新后的驱动会自动下载。
- **内置 AI 引擎：** 解压引擎时只保留文件的读取、写入和执行权限。
- **计划任务，安全：** `{results}` 和 `{alerts}` 现在只会填入邮件和消息的文本。脚本的 SQL、文件或文件夹名称、地址以及附件路径不再使用它们，因此回显了恶意构造数据的数据库错误不会变成 SQL 或路径。
- **计划任务消息：** 消息或邮件中的 `{results}` 和 `{alerts}` 现在使用 DBine 的界面语言，而不再总是西班牙语，即使任务在 DBine 关闭时运行也是如此。
- **笔记本，大型 DataFrame SQL 结果：** **高级导出… › 全部** 会写入完整结果，而不只是显示的行。单元格的 SQL 会在 DuckDB 中针对其上次运行读取的 DataFrame 重新运行，无需重新读取文件或再次查询数据源。其图表默认基于完整结果计算，也可以关闭。结果被截断时，网格还会提示排序和 **复制全部** 仅作用于已加载的行。

## [1.0.0] - 2026-10-19

### 新增
- **浅色主题：** 新增浅色主题和 **跟随系统** 选项（默认），会随操作系统的切换而变化；可用 Cmd/Ctrl + “+”“−”“0” 或鼠标滚轮缩放界面，并可在 **设置 › 常规** 中分别设置编辑器和结果网格的字体大小。
- **Vim 模式和自定义快捷键：** SQL 编辑器支持 Vim 模式（设置 › 常规），每个键盘快捷键都可以在 **设置 › 键盘快捷键** 中修改，支持搜索、重置和冲突提示。
- **拆分编辑器：** 最多可并排或上下排列三个编辑器组（标签页菜单、标签栏按钮或 Cmd/Ctrl+\）；可在组之间拖动标签页，或将其拖到某个组的右半部分或下半部分来拆分。重启后会恢复布局。
- **托管数据库的云登录：** 连接到 Amazon RDS 和 Aurora、Google Cloud SQL 和 AlloyDB 以及 Azure Database 的 PostgreSQL 和 MySQL 连接，可使用 **AWS IAM**、**Google Cloud IAM** 或 **Microsoft Entra ID** 登录。DBine 每次连接前都会通过您的 AWS 配置文件（包括 SSO）、gcloud 或 az 生成新令牌，无需再粘贴 15 分钟就过期的令牌，**测试** 还会显示是哪个身份登录的。
- **从命令或密钥管理器获取密码：** 任何引擎的任何密钥字段，以及 SSH 隧道的密码和密钥口令，都可以从命令、1Password、HashiCorp Vault、AWS Secrets Manager、Azure Key Vault 或 Google Secret Manager 获取值。每次连接时获取，仅保存在内存中，支持 JSON 键选项和按字段测试（只显示长度）。来自导入或已恢复备份的命令或密钥管理器设置，只有在您于此电脑上确认后才会运行。
- **KeePass 和 Zoho Vault 作为密码来源：** 任何密钥字段（包括 SSH 隧道的）都可以从 KeePass 数据库（.kdbx，由 DBine 自行读取；主密码来自钥匙串，连接时询问或通过命令获取；可选密钥文件）的条目，或 Zoho Vault 的密钥（任意数据中心，在您的电脑上用主密码短语解密）获取值，也可以从同一条目获取连接的用户。
- **Vault 数据库凭据、CyberArk 和 Delinea 作为密码来源：** 连接密码可来自 HashiCorp Vault 的数据库密钥引擎（为连接生成凭据，连接打开期间续期，关闭时吊销；Vault 也支持 AppRole 登录）、CyberArk Conjur、CyberArk 的 Central Credential Provider（使用客户端证书）以及 Delinea Secret Server。
- **Bitwarden、Bitwarden Secrets Manager 和 AWS Parameter Store 作为密码来源：** 任何密钥字段（包括 SSH 隧道的）都可以从 Bitwarden 项目（个人的或组织的，位于 bitwarden.com、bitwarden.eu 或您自己的服务器，在您的电脑上用主密码解密）、Bitwarden Secrets Manager 的密钥（使用机器账户的访问令牌）或 AWS Systems Manager 参数（SecureString、String 或 StringList 的一个元素）获取值，也可以带上 JSON 键，以及从同一项目获取连接的用户。
- **先选凭据，并浏览密钥管理器：** 连接表单首先询问 **凭据**：把密码保存在钥匙串中、每次都询问，或从命令行或密钥管理器获取用户和密码（其字段会取代它们）。**浏览** 按钮会列出管理器的条目及其字段名，让您直接选择而无需手动输入引用；它只显示名称，从不显示值。
- **从条目获取用户和密码：** 当凭据来自 KeePass、Bitwarden、1Password、Zoho Vault、Delinea 或 CyberArk CCP 时，用户和密码都取自该条目，并且 **高级设置** 允许您从条目自己的列表中选择其他字段。每种来源先询问登录信息，最后才询问条目，填写完成后即可使用浏览。
- **Zoho Vault 账户，连接一次即可：** 引导式的 **连接 Zoho Vault** 向导会带您走过 Zoho 控制台，找到您的数据中心并检查主密码短语；之后任何连接只需选择该账户。账户保存在此电脑上，在 **设置 › 密钥管理器** 中管理。
- **每个密钥管理器的账户：** Bitwarden、Bitwarden Secrets Manager、HashiCorp Vault 和 OpenBao、Delinea、CyberArk Conjur 和 Central Credential Provider、1Password、AWS、Azure Key Vault、Google Secret Manager 和 KeePass 现在都可以在 **设置 › 密钥管理器** 中通过引导式向导一次性连接：向导会说明每个值在哪里，实时检查账户并显示是谁登录的；之后任何连接只需选择该账户，凭据只会发送到该管理器自己的服务器。
- **可选择的 Azure 租户：** Azure Key Vault 账户向导会列出您的 Microsoft Entra 租户（仍可手动输入），当您尚未登录所选租户时，会显示可复制的 `az login --tenant` 命令。
- **DataFrame SQL 单元格中的自动补全：** 上方的单元格会作为表提示（即使尚未运行），在 `name.` 或别名之后以及 select 列表中提示其列，并在 FROM 和 JOIN 之后提示 DuckDB 的文件读取函数；选择读取函数后，光标会停在其引号之间，**选择文件…** 会打开文件对话框。笔记本刷新时，FROM 之后的表列表不再自行关闭。
- **地图视图：** 含几何列（PostGIS、MySQL、SQL Server、SQLite/SpatiaLite、DuckDB、MongoDB、Elasticsearch 等）或经纬度列的结果可以显示在地图上，地图在本机绘制，底图为内置的离线世界地图。可在 **设置 › 地图** 中启用详细的在线地图图块；此时提供商能看到您正在查看的区域，但看不到数据。
- **Oracle 上地图：** SDO_GEOMETRY 列现在会显示在结果地图上，无论是表数据还是您自己的查询（在服务器上连同 SRID 转换为 WKT）；这些单元格在网格中为只读。
- **直接打开数据文件：** SQLite、DuckDB、CSV、TSV、Parquet、JSON 和 Excel 文件可通过双击或“用 DBine 打开”、拖放到窗口上，或 **文件 › 打开文件…**（Cmd/Ctrl+O）打开。每个文件都作为临时连接打开，可用 **保存连接** 保留，数据文件会立即显示前 1,000 行。
- **命令面板：** Cmd/Ctrl+Shift+P 可查找并运行任何操作，只显示当前引擎支持的内容；**转到…**（Cmd/Ctrl+P）可跳转到连接、数据库、表、视图、例程、已保存的查询和打开的标签页。每个面板操作也可以设置自己的快捷键。
- **跳转到相关行：** 在表数据或查询结果中，**转到被引用的行**（或 Cmd/Ctrl+点击）会打开按该外键筛选的父表，**引用它的行** 会打开按该值筛选的每个子表，复合键也支持。适用于所有支持外键的引擎。
- **SQL 笔记本：** **文件 › 新建笔记本** 会打开一个笔记本：SQL 单元格可在任何连接上运行，每个单元格下方保留自己的结果和图表，另有文本单元格用于说明。它保存为标准的 Jupyter `.ipynb`，GitHub、VS Code 和 Jupyter 都能显示，Azure Data Studio 和 Jupyter `%%sql` 笔记本也能打开。文件保留每个结果的预览（每个单元格 100 行和 1 MB，可在 **设置 › 笔记本** 中调整），包含总行数、时间和耗时，绝不包含连接详情或密码；在项目内，笔记本默认不保存结果，除非您启用。助手可以在看不到行数据的情况下解释结果、建议下一个单元格或大型查询的分组版本。可导出为 HTML 或 Markdown。
- **基于完整结果的图表：** 图表基于结果的所有行计算，而不仅是网格中已加载的行。在 SQL 引擎上，分组在服务器上运行；在笔记本的 DataFrame SQL 单元格中由 DuckDB 完成；在其他引擎上由 DBine 完成。
- **更多图表类型：** 直方图、热力图、箱线图（含须线和离群值）、柱线组合图以及 KPI（整个结果的总计）。
- **每个笔记本单元格可有多个图表：** 笔记本单元格可以显示其结果的多个图表，并记住其地图使用的数据源。
- **笔记本导出中的图表：** 导出为 HTML 或 Markdown 的笔记本会以图片形式包含其图表。
- **在笔记本中联接不同数据库的结果：** 每个单元格的结果都是一个以该单元格命名的 DataFrame（名称显示在标题中且可编辑），**DataFrame SQL** 单元格无需连接，即可用 DuckDB 的 SQL 把上方的 DataFrame 当作表来查询和联接，因此一个单元格可以把 PostgreSQL 的结果与 SQL Server 和 MongoDB 的结果联接起来。它运行在内存中的 DuckDB 数据库里，不能写入文件或访问网络，笔记本关闭时即被丢弃；每个来源的行来自它在本次会话中的运行，或在其自己的连接上以只读方式重新运行的查询（每个 DataFrame 最多 1,000,000 行，可在 **设置 › 笔记本** 中调整）。
- **笔记本作为定时报告：** 定时任务可以在 DBine 关闭时运行笔记本（**运行笔记本**），保存运行后的副本并导出为 HTML 或 Markdown，随后的 **发送电子邮件** 步骤可以将其作为附件，或把 HTML 导出作为邮件正文发送。无人值守时不会询问任何内容：只读取该笔记本已设为 **始终允许** 的文件，在需要确认的环境中只有步骤明确授权才会运行，会更改数据的单元格则并入任务的审批。定时导出包含表格；从应用内导出时包含图表。
- **DataFrame SQL 单元格中的本地文件：** DataFrame SQL 单元格可以读取您电脑上的 CSV、Parquet、JSON 或 NDJSON 以及 Excel 文件，使用 `read_csv`、`read_parquet`、`read_json` 和 `read_xlsx`，支持 `data/*.csv` 之类的模式和文件列表；用 `st_read` 读取地理文件（GeoJSON、带配套文件的 Shapefile、GeoPackage、KML、GPX、FlatGeobuf）；用 `sqlite_scan` 和 `read_duckdb` 读取 SQLite 和 DuckDB 数据库文件中的表。每个都是一个 DataFrame，可与任何引擎的单元格结果联接；几何数据仍可使用 `ST_Contains` 等空间函数并显示在地图上。文件只读取，绝不写入。首次读取文件前，笔记本会询问：仅本次运行允许、在此电脑上对该笔记本始终允许（模式也涵盖新文件），或拒绝；凭据、密钥、Shell 历史和系统文件绝不会被读取。如果文件内容自上次运行后发生了变化，结果会予以提示。设置 › 笔记本会列出每个笔记本可读取的文件，并可将其移除。相同的读取函数也能读取来自网络（`https://`）以及 Amazon S3、Google Cloud Storage 和 Azure Blob Storage（`s3://`、`gs://`、`az://`、`abfss://`，支持 `s3://bucket/2026/*.parquet` 之类的模式）的文件，审批方式相同，并会先显示服务器；DBine 自行将文件下载到读取后即删除的私有临时文件夹，使用此电脑的 AWS、Google Cloud 或 Azure 登录（可在设置 › 笔记本中选择 AWS 配置文件），绝不访问本地或私有网络地址，不会跟随重定向到其他服务器，也不接受 URL 中的凭据。在其他程序中打开的 GeoPackage 现在也会包含仍在其 `-wal` 中的更改。
- **将笔记本导出为 Python 笔记本：** **导出 › Python 笔记本（.ipynb）** 会写出一个可复现该笔记本的 Jupyter 笔记本：每个连接一个 SQLAlchemy 引擎（或引擎自己的 Python 客户端，如 pymongo 或 redis），每个查询单元格读入以该单元格命名的 pandas DataFrame，DataFrame SQL 单元格用 DuckDB 在这些 DataFrame 上运行，文本单元格则变为 Markdown。密码和令牌绝不会导出：代码从环境变量读取它们，第一个单元格会列出这些变量和需要安装的软件包。除非您选择包含预览，否则不含结果。
- **在笔记本中将 DataFrame 写入表：** **写入单元格**（用任意单元格下的 **写入表…** 添加）会把一个单元格的完整结果写入 SQL Server、PostgreSQL、MySQL、MariaDB、Oracle、SQLite、DuckDB、MongoDB 等的表：插入、按键更新、插入或更新，或替换表内容，并支持列映射、预览，以及按需新建表。每次写入都会先询问，显示写到哪里、如何写以及多少行（生产环境连接需要输入表名），在支持事务的引擎上于单个事务中运行，只读连接绝不会被写入。在定时任务中，写入单元格仅在步骤授权了该单元格、表和模式时才会运行。
- **笔记本参数：** 笔记本可以定义参数（文本、数字、日期、日期和时间、布尔值或固定列表，带默认值），其单元格以 `{{name}}` 使用；运行时会询问缺少的值，每个值都作为数据传给数据库（由驱动程序绑定，或按引擎自身规则加引号），每个结果都会显示产生它的参数值。
- **新增五种语言：** DBine 及其手机应用现在支持德语、俄语、简体中文、日语和韩语，此外还有西班牙语、英语、葡萄牙语、法语和意大利语；语言跟随您的系统，并且只加载正在使用的那一种。

### 改进
- **macOS 版已签名：** macOS 应用使用 Apple Developer ID 签名并经 Apple 公证，因此打开时不会出现“无法验证开发者”的警告，也无需任何终端命令。已签名版本首次打开时，macOS 会询问一次 DBine 是否可以使用其在钥匙串中的密钥：请选择 **始终允许**。
- **云登录只信任实例的证书：** 使用 AWS IAM、Google Cloud IAM 或 Microsoft Entra 登录的连接，以及使用实例 CA 文件的 Cloud SQL 和 AlloyDB 连接，现在在发送令牌或密码之前只接受该 CA（或 Amazon 的 RDS 证书包），而不是任何公共证书。当 **信任服务器证书** 开启时，要求以明文发送密码的服务器现在会连接失败，而不会收到密码。
- **Windows 版已签名：** Windows 安装程序和应用使用代码签名证书签名，因此 Windows 会显示 DBine 的发布者，而不是“未知发布者”。

### 修复
- **服务器监视器：** 其数字、表格和说明在所有引擎中均已翻译，不再有部分标签显示为西班牙语。
- **AI 助手和优化器：** 发给模型的指令使用界面语言，因此本地模型会用该语言回答，而不是西班牙语。说明已向模型发送了什么内容的那一行也已翻译。
