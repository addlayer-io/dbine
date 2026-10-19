# DBine translation glossary

Spanish (rioplatense, voseo) is the source. Use these terms everywhere (UI
files `web/src/locales/<lang>/*.json` and backend messages `backend.json`).
Tone: a professional database tool, short and direct; sentence case (only the
first word capitalized, like the Spanish). Keep unchanged: engine and product
names (SQL Server, PostgreSQL, DuckDB…), SQL keywords and code, identifiers,
file names, keyboard shortcuts (⌘C, ⇧⌥F), placeholders (`{0}`, `{{name}}`),
units, and anything in backticks.

| es | en | pt (Brasil) | fr | it |
|---|---|---|---|---|
| base de datos / base | database | banco de dados / banco | base de données / base | database |
| motor | engine | motor | moteur | motore |
| conexión | connection | conexão | connexion | connessione |
| servidor | server | servidor | serveur | server |
| tabla | table | tabela | table | tabella |
| vista | view | view | vue | vista |
| columna | column | coluna | colonne | colonna |
| fila | row | linha | ligne | riga |
| esquema | schema | esquema | schéma | schema |
| índice | index | índice | index | indice |
| clave primaria | primary key | chave primária | clé primaire | chiave primaria |
| clave foránea | foreign key | chave estrangeira | clé étrangère | chiave esterna |
| restricción | constraint | restrição | contrainte | vincolo |
| procedimiento | procedure | procedimento | procédure | procedura |
| función | function | função | fonction | funzione |
| disparador (trigger) | trigger | trigger | déclencheur | trigger |
| secuencia | sequence | sequência | séquence | sequenza |
| query / consulta | query | consulta | requête | query |
| resultado(s) | result(s) | resultado(s) | résultat(s) | risultato/i |
| ejecutar | run | executar | exécuter | eseguire |
| plan de ejecución | execution plan | plano de execução | plan d'exécution | piano di esecuzione |
| solo lectura | read-only | somente leitura | lecture seule | sola lettura |
| explorador | explorer | explorador | explorateur | esplora risorse |
| pestaña | tab | aba | onglet | scheda |
| carpeta | folder | pasta | dossier | cartella |
| biblioteca | library | biblioteca | bibliothèque | libreria |
| script | script | script | script | script |
| respaldo / backup | backup | backup | sauvegarde | backup |
| frase clave | passphrase | frase secreta | phrase secrète | passphrase |
| llavero del sistema | system keychain | chaveiro do sistema | trousseau du système | portachiavi di sistema |
| sincronización | sync | sincronização | synchronisation | sincronizzazione |
| migrar / migración | migrate / migration | migrar / migração | migrer / migration | migrare / migrazione |
| comparar esquemas | compare schemas | comparar esquemas | comparer les schémas | confronta schemi |
| sincronizar (esquemas) | sync | sincronizar | synchroniser | sincronizzare |
| diseñador | designer | designer | concepteur | designer |
| diagrama | diagram | diagrama | diagramme | diagramma |
| monitor | monitor | monitor | moniteur | monitor |
| Profiler | Profiler | Profiler | Profiler | Profiler |
| captura completa / muestreo | full capture / sampling | captura completa / amostragem | capture complète / échantillonnage | cattura completa / campionamento |
| asistente (de IA) | (AI) assistant | assistente (de IA) | assistant (IA) | assistente (IA) |
| modelo integrado | built-in model | modelo integrado | modèle intégré | modello integrato |
| etiqueta (tag) | tag | tag | étiquette | tag |
| filtro | filter | filtro | filtre | filtro |
| duración | duration | duração | durée | durata |
| usuario | user | usuário | utilisateur | utente |
| aplicación | application | aplicativo | application | applicazione |
| cliente (host) | client | cliente | client | client |
| aviso / advertencia | warning | aviso | avertissement | avviso |
| descargar | download | baixar | télécharger | scaricare |
| Configuración | Settings | Configurações | Paramètres | Impostazioni |

## German, Russian, Chinese, Japanese, Korean

A starter glossary for the languages added after 1.0.0. Register and style
per language: `.claude/agents/translator.md`. Stay in Latin script in every
language: SQL, DuckDB, JSON, CSV, engine and product names, DBine, Profiler,
code and identifiers.

| en | de | ru | zh (简体) | ja | ko |
|---|---|---|---|---|---|
| database | Datenbank | база данных | 数据库 | データベース | 데이터베이스 |
| engine | Engine | движок | 引擎 | エンジン | 엔진 |
| connection | Verbindung | подключение | 连接 | 接続 | 연결 |
| server | Server | сервер | 服务器 | サーバー | 서버 |
| table | Tabelle | таблица | 表 | テーブル | 테이블 |
| view | Sicht | представление | 视图 | ビュー | 뷰 |
| column | Spalte | столбец | 列 | 列 | 열 |
| row | Zeile | строка | 行 | 行 | 행 |
| schema | Schema | схема | 架构 | スキーマ | 스키마 |
| index | Index | индекс | 索引 | インデックス | 인덱스 |
| primary key | Primärschlüssel | первичный ключ | 主键 | 主キー | 기본 키 |
| foreign key | Fremdschlüssel | внешний ключ | 外键 | 外部キー | 외래 키 |
| constraint | Einschränkung | ограничение | 约束 | 制約 | 제약 조건 |
| procedure | Prozedur | процедура | 存储过程 | プロシージャ | 프로시저 |
| function | Funktion | функция | 函数 | 関数 | 함수 |
| trigger | Trigger | триггер | 触发器 | トリガー | 트리거 |
| sequence | Sequenz | последовательность | 序列 | シーケンス | 시퀀스 |
| query | Abfrage | запрос | 查询 | クエリ | 쿼리 |
| result(s) | Ergebnis(se) | результат(ы) | 结果 | 結果 | 결과 |
| run | ausführen | выполнить | 运行 | 実行 | 실행 |
| execution plan | Ausführungsplan | план выполнения | 执行计划 | 実行計画 | 실행 계획 |
| read-only | schreibgeschützt | только для чтения | 只读 | 読み取り専用 | 읽기 전용 |
| explorer | Explorer | обозреватель | 资源管理器 | エクスプローラー | 탐색기 |
| tab | Tab | вкладка | 标签页 | タブ | 탭 |
| folder | Ordner | папка | 文件夹 | フォルダー | 폴더 |
| library | Bibliothek | библиотека | 库 | ライブラリ | 라이브러리 |
| script | Skript | скрипт | 脚本 | スクリプト | 스크립트 |
| backup | Sicherung | резервная копия | 备份 | バックアップ | 백업 |
| export / import | exportieren / importieren | экспорт / импорт | 导出 / 导入 | エクスポート / インポート | 내보내기 / 가져오기 |
| notebook | Notebook | блокнот | 笔记本 | ノートブック | 노트북 |
| credential(s) | Anmeldedaten | учётные данные | 凭据 | 資格情報 | 자격 증명 |
| secret manager | Secret-Manager | менеджер секретов | 密钥管理器 | シークレットマネージャー | 시크릿 관리자 |
| system keychain | Systemschlüsselbund | системная связка ключей | 系统钥匙串 | システムのキーチェーン | 시스템 키체인 |
| passphrase | Passphrase | парольная фраза | 密码短语 | パスフレーズ | 암호 문구 |
| sync | Synchronisierung | синхронизация | 同步 | 同期 | 동기화 |
| migration | Migration | миграция | 迁移 | 移行 | 마이그레이션 |
| compare schemas | Schemas vergleichen | сравнить схемы | 比较架构 | スキーマを比較 | 스키마 비교 |
| designer | Designer | конструктор | 设计器 | デザイナー | 디자이너 |
| diagram | Diagramm | диаграмма | 关系图 | ダイアグラム | 다이어그램 |
| monitor | Monitor | монитор | 监视器 | モニター | 모니터 |
| Profiler | Profiler | Profiler | Profiler | Profiler | Profiler |
| (AI) assistant | KI-Assistent | ИИ-ассистент | AI 助手 | AI アシスタント | AI 어시스턴트 |
| filter | Filter | фильтр | 筛选器 | フィルター | 필터 |
| user | Benutzer | пользователь | 用户 | ユーザー | 사용자 |
| download | herunterladen | скачать | 下载 | ダウンロード | 다운로드 |
| warning | Warnung | предупреждение | 警告 | 警告 | 경고 |
| Settings | Einstellungen | Настройки | 设置 | 設定 | 설정 |
