# Alterações

## [0.1.10] - 2026-10-10

### Novidades
- **Os drivers se atualizam sozinhos, separados do app:** o DBine procura em um índice assinado o driver mais novo compatível com a sua versão, baixa em segundo plano e volta ao anterior se algo falhar. Em Configurações › Drivers há um botão **Buscar atualizações**, o estado de cada driver e **Voltar à anterior**. Um driver pode ser publicado sozinho, sem uma nova versão do app.
- **Renomear um banco de dados:** **Renomear…** em um banco de dados no explorador. O diálogo mostra as outras sessões abertas nele (que a renomeação encerra), se o novo nome já existe, quantos objetos são movidos e o script completo. Depois, o banco padrão da conexão, as abas abertas, as consultas salvas, as migrações, os destinos de projetos e as etapas de tarefas agendadas passam a usar o novo nome; as tarefas que alteram dados nele pedem aprovação de novo. Disponível no SQL Server, Azure SQL, Babelfish, na família PostgreSQL, no MySQL, MariaDB, Snowflake e MongoDB; onde o motor não consegue renomear (ou mover) um banco de dados, a opção não é oferecida.
- **A qual coluna corresponde este valor?** Em um `INSERT … VALUES`, ao colocar o cursor sobre um valor aparece um tooltip com a sua coluna (por exemplo "Coluna 14 de 48: Name") e essa coluna é destacada na lista. Sem lista de colunas, usa as colunas da tabela em ordem. Funciona em todos os motores SQL e CQL.
- **Mais formas de entrar no SQL Server com o Microsoft Entra ID:** interativa com MFA (no navegador), integrada (a conta do Windows, pelo ADFS federado da organização), identidade gerenciada e padrão (variáveis de ambiente, identidade gerenciada, Azure CLI ou Azure Developer CLI). O token é mantido enquanto o DBine está aberto e renovado antes de expirar.
- **Associar um login a um banco:** **Associar login…** em Usuários e permissões cria o usuário do banco para um login do servidor que já existe (`CREATE USER … FOR LOGIN`, com o schema padrão), listando os logins que ainda não têm usuário ali. Disponível no SQL Server, Azure SQL Database (lá o login é digitado), Babelfish e SAP ASE.

### Melhorias
- **O que cada versão traz:** o aviso de nova versão mostra as mudanças dela e das versões intermediárias, a partir da que você tem instalada, no idioma do app.
- **Versões antigas do app:** a partir de agora, um app mais antigo que as últimas cinco versões precisa ser atualizado para baixar novos drivers. Os drivers que ele já tem instalados continuam funcionando.
- **Usuários e permissões:** as alterações entram em uma fila e são aplicadas juntas. Uma barra de **Alterações pendentes** as lista, uma única revisão mostra o script completo e elas rodam uma de cada vez: as aplicadas saem da lista e, se uma falhar, ela e as seguintes continuam pendentes.
- **As leituras dos clientes MCP e do assistente de IA são protegidas pelo próprio banco de dados:**
  - **Garantido pelo servidor:** na família PostgreSQL, no MySQL, MariaDB, SQLite e libSQL, cada leitura roda como uma única instrução dentro de uma transação somente leitura que o servidor faz valer e que o DBine sempre reverte.
  - **Aprovação:** nos demais motores, cada leitura pede a sua aprovação no DBine, do mesmo jeito que as escritas. O diálogo de aprovação informa se é uma leitura ou uma escrita, e **Aprovar tudo** vale separadamente para leituras e escritas.
  - **Verificação de escrita:** o DBine continua verificando cada consulta em busca de escritas, como uma camada extra.

### Correções
- **O somente leitura é mais rigoroso:** as consultas dos clientes MCP com nível Leitura, do assistente de IA e das conexões somente leitura são verificadas palavra por palavra, não só pela primeira. Agora elas recusam:
  - uma escrita escondida depois de uma leitura em um lote do SQL Server;
  - um `WITH` que modifica dados;
  - `SELECT … INTO`, `EXEC` e `SET`;
  - funções que atuam fora da consulta, como `set_config`, `dblink_exec` e `xp_cmdshell`.

  Um plano estimado recusa scripts que desligariam o modo plano, então as alternativas de IA de **Otimizar consulta** não conseguem executar nada. As tarefas agendadas cujos scripts agora contam como escrita pedem aprovação de novo.
- **Biblioteca com git:** um repositório compartilhado não pode mais fazer o DBine ler, gravar ou apagar arquivos fora da pasta da Biblioteca.
- **Exportação SQL:** os valores de texto são escapados do jeito que o motor de origem os lê, então um valor armazenado não consegue adicionar instruções a um script `INSERT` para MySQL, ClickHouse, BigQuery, Hive, Spark ou Databricks.
- **Túneis SSH:** a chave de host de cada servidor é verificada separadamente. Uma chave aceita para um salto não vale mais para o servidor seguinte, o `known_hosts` é consultado primeiro e uma chave alterada é sempre recusada. Os servidores que você aceitou antes são perguntados mais uma vez. A porta local do túnel só atende aos programas do seu próprio usuário.
- **Backup na nuvem:** um arquivo de backup modificado por outra pessoa não pode mais enfraquecer a criptografia do seu próximo envio.
- **libSQL / Turso:** um `authToken` em uma URL colada é guardado no chaveiro do sistema, não no endereço, no nome nem no histórico da conexão. As conexões salvas antes são limpas quando o DBine inicia.
- **Documentar o banco de dados:** nomes de coluna não podem injetar HTML no dicionário de dados em Markdown.
- **Notificações de tarefas no Windows:** uma mensagem de erro do banco não pode mais executar comandos pela notificação. As notificações no macOS e no Linux também recebem o texto como argumentos separados.
- **Túneis SSH:** um servidor cuja chave você aceitou no DBine e que agora apresenta uma diferente é recusado, em vez de perguntar de novo. A seção SSH da conexão lista os servidores aceitos, cada um com **Esquecer**.
- **SQL no PostgreSQL:** os valores de texto são escritos como `E'…'` com as barras invertidas escapadas, então um valor não consegue encerrar a string antes da hora em um servidor com `standard_conforming_strings` desativado. Isso cobre a família PostgreSQL, o CockroachDB e motores semelhantes.
- **Scripts do Snowflake:** uma barra invertida dentro de um `"nome entre aspas"` não muda mais onde uma instrução termina.
- **Exportação CSV e TSV:** células de texto e nomes de coluna que começam com `=`, `+`, `-`, `@`, uma tabulação ou um retorno de carro recebem um `'` na frente, para que as planilhas não os executem como fórmulas. Os números nunca são alterados. Uma opção no diálogo de exportação desativa isso.
- **Descartar alterações em Projetos:** um arquivo com nome de padrão (`*`) descarta apenas esse arquivo.
- **Copiar um subconjunto:** a mascaração usa uma nova chave aleatória de 256 bits a cada execução.
- **Atualizações de drivers:** o app nunca aceita um índice de drivers mais antigo que o da sua versão, nem mesmo em uma instalação nova, nem um que deixou de ser renovado. Os drivers instalados continuam funcionando em ambos os casos.
- A importação de conexões, o linter e a verificação de saúde não param mais com caracteres acentuados ou outros de vários bytes.
- **Somente leitura no SQL Server:** cada consulta roda em uma transação que sempre é revertida. Backups, restaurações, ativar ou desativar triggers, escritas com ponteiros de texto, Service Broker e instruções de transação são recusados quando vêm depois de uma leitura no mesmo lote.
- **Somente leitura no PostgreSQL:** nomes escritos com escapes Unicode (`U&"…"`) são recusados, para que uma função proibida não possa ser chamada com outra grafia.
- **Exportação CSV e TSV:** a proteção contra fórmulas também vale para texto guardado em colunas declaradas como numéricas, o que o SQLite permite.
- **Scripts gerados:** nomes de objetos vindos do servidor não podem encerrar um comentário e rodar como código. Isso cobre as correções sugeridas pela **Verificação de integridade** e os scripts de usuários, backups e estrutura. Nomes do ClickHouse com barra invertida são colocados entre aspas corretamente.
- **Somente leitura:** uma instrução que não começa com uma palavra, como um nome entre colchetes que executa um procedimento no SQL Server, é recusada. As palavras dentro de leituras entre parênteses também são verificadas.
- **Exportação SQL:** a partir do MySQL, ClickHouse e outros motores que interpretam barras invertidas, as aspas são escritas como `''`, de modo que o script é lido igual em qualquer destino.
- **Modificar tabela:** os avisos do script que você abre como consulta ficam na própria linha de comentário.
- **Senhas e opções secretas** são mascaradas em todos os lugares onde são editadas, inclusive nas etapas de backup das tarefas agendadas.
- **Ver dependências e Renomear** não travam mais em rotinas com nomes entre aspas incomuns.
- **Somente leitura no SQL Server:** um lote só pode começar com `SELECT`, `WITH`, `USE` ou `PRINT`. O que vem depois de `SHOW`, `DESCRIBE` ou `PRAGMA` é verificado em todos os motores.
- **Importar conexões:** uma URL JDBC do Oracle com usuário e senha mantém esses dados fora do nome da conexão: a senha vai para o chaveiro do sistema.
- **Somente leitura no SQL Server:** um procedimento cujo nome começa como `print_` ou `select1` deixa de ser tomado por uma leitura.
- **Importar conexões:** uma senha do Oracle com `@` é guardada inteira no chaveiro do sistema.
- **Túneis SSH no Linux:** a porta local do túnel só confia em conexões abertas pelo seu próprio usuário.
- **Somente leitura:** uma consulta com um retorno de carro isolado ou um espaço Unicode incomum é recusada, porque os motores discordam sobre onde um comentário ou uma instrução termina ali. Funções com efeitos colaterais também são recusadas: `load_extension` no SQLite, `pg_notify` e leituras de replicação lógica no PostgreSQL, e o cancelamento de sessões ou consultas no Snowflake.
- **Exportação SQL** de uma grade de vários bancos de dados: escapa as strings para que o script seja lido da mesma forma em qualquer motor.
- **Importar conexões:** uma URL do SQL Server sem host não coloca mais a senha no nome da conexão.
- **Usuários e permissões:** a prévia oculta as senhas do PostgreSQL em todas as formas.
- **Somente leitura:** consultas que adquirem bloqueios pelos quais outras sessões esperam são recusadas, por exemplo `pg_advisory_lock`, `GET_LOCK`, `LOCK IN SHARE MODE` e `FOR SHARE`.
- **Snowflake e Cassandra:** os comentários `//` são lidos como o servidor os lê.
- **As conexões somente leitura do SQL Server** recusam hints de bloqueio e `WAITFOR`.
- **As conexões somente leitura** recusam avançar uma sequência (`NEXT VALUE FOR`, `.NEXTVAL`).
- **Renomear:**
  - Em todos os motores, é recusado um dependente cujo código seria dividido em mais de uma instrução.
  - Corpos de rotinas do Snowflake que contêm `$$` são mantidos inteiros.
  - Nomes do OrientDB que o DBine não conseguiria colocar entre aspas com segurança são recusados.

## [0.1.9] - 2026-10-09

### Novidades
- **Renomear com impacto:** **Renomear…** no explorador altera o nome de uma tabela, view, rotina, coluna, índice ou esquema e, no mesmo script, reescreve as views, procedimentos, funções e triggers que o usam. Antes de executar, mostra o que o motor atualiza sozinho, o que é reescrito e o que precisa ser revisado manualmente (SQL dinâmico, código ilegível), junto com o script completo. Roda em uma transação onde o motor permite. Está em todos os motores que podem renomear algo; os limites de cada um estão em `docs/engine-support.md`.
- **Modificar uma tabela:** **Modificar…** abre o designer sobre uma tabela existente e monta o `ALTER` do motor. Mantém o que o designer não mostra (CHECKs, opções de índices, ordem das colunas da chave) e recria as views e triggers que dependem da tabela. Renomear uma coluna ali passa pela revisão de impacto; em conexões de produção, pede para digitar o nome da tabela antes de executar.
- **Histórico por consulta:** a barra de **Histórico** acompanha a aba ativa, como uma linha do tempo: versões da consulta salva com diferenças e restauração, suas execuções e, em arquivos de um projeto, seus commits do git.
- **Navegação no editor:** Cmd/Ctrl+clique em uma tabela, view ou rotina abre sua estrutura ou definição, e **Mostrar no explorador** a localiza na árvore. Tabelas e colunas que não existem são marcadas antes de executar.
- **Parâmetros nas consultas:** `:nome` e `?` são solicitados ao executar, e o último valor é lembrado.
- **Snippets** por motor (por exemplo, `sel` + Tab) e menu do botão direito no editor.
- **Totais da seleção:** ao selecionar células da grade, são exibidos quantidade, soma, média, mínimo e máximo.
- **Tarefas agendadas:** scripts, exportações, comparação de esquemas, backups, **Documentar o banco de dados** e **Enviar um e-mail** (SMTP) que rodam com o DBine fechado, por meio do agendador do sistema. Com notificações por tarefa e histórico de execuções. O que altera dados é aprovado explicitamente.
- **Qualidade de código:** regras por motor no editor e **Ver problemas**.
- **Documentar o banco de dados:** dicionário de dados em HTML ou Markdown, com diagrama, linhas estimadas e comentários de views e rotinas. As linhas estimadas e os comentários vêm dos metadados do motor, sem ler tabelas nem consumir cota nos motores em nuvem.
- **Projetar consulta:** construtor visual de consultas.
- **Copiar um subconjunto** de dados, com mascaramento.
- **Otimizar consulta:** reescritas, índices sugeridos, alternativas da IA e comparação medida. As alternativas da IA são validadas contra o plano estimado do banco antes de serem exibidas.
- **Verificação de integridade** de um banco, em todos os motores, com verificações próprias no SQL Server, na família PostgreSQL, na família MySQL, no Oracle, SAP HANA, Firebird, ClickHouse, Snowflake, BigQuery, Databricks e nos perfis ODBC.
- **Buscar no banco:** nomes de objetos, código de views e rotinas, e nomes de colunas (com sua tabela e tipo).
- **Gerar dados de teste** para uma tabela.
- **Propriedades do banco** e opções avançadas ao criar um banco, em abas e por motor, com pré-visualização do script.
- **Visualização JSON em árvore** dos resultados, com edição, e **Adicionar linha** / **Adicionar documento** na aba Dados e na grade.
- **Nova marca:** o logotipo com o halo.

### Melhorias
- A cor da conexão aparece como uma faixa na borda da linha, e o ponto indica apenas o estado (verde conectada, vermelho desconectada).

### Correções
- A comparação de esquemas não é mais cancelada por leituras do explorador, e o SQL Server se reconecta.
- As linhas de conexão sem cor ficam alinhadas com as que têm cor.
- Arrastar tabelas para o construtor de consultas funciona no macOS.
- Propriedades do SQL Server: nomes de arquivo longos não transbordam o diálogo, e a aba "Opções ANSI e de segurança" está traduzida.
- O editor não marca mais como desconhecidas as colunas de uma subconsulta com alias.

### Já disponível
- **Executar uma consulta em vários bancos ao mesmo tempo:** escolhe-se um ou vários bancos de uma conexão, e os resultados são unidos com uma coluna que indica o banco de cada linha. Chegou na 0.1.4. Veja `docs/multi-database-queries.md`.

## [0.1.8] - 2026-10-06

### Novidades
- **PostgreSQL atrás de gateways que só aceitam o protocolo simples:** a conexão tem uma nova opção, **Protocolo de consultas**: Automático ou Somente protocolo simples. Serve para gateways que rejeitam o protocolo estendido com o erro 0A000. Nesse modo, o que precisa do protocolo estendido avisa com uma mensagem clara em vez de falhar.

### Correções
- **Nova consulta com a aba Nova conexão aberta** falhava com "FOREIGN KEY constraint failed". Agora a consulta abre na última conexão que você tinha aberta, ou pede para escolher um banco no explorador.

## [0.1.7] - 2026-10-05

### Correções
- **Comparar dados com colunas identity:** sincronizar linhas para uma tabela do SQL Server com uma coluna `IDENTITY` falhava com "Cannot insert explicit value for identity column". Agora o DBine ativa `IDENTITY_INSERT` apenas enquanto insere essas linhas.
- No PostgreSQL, depois de copiar linhas com seus ids, a sequência avança para que o próximo insert não colida com um id copiado.

## [0.1.6] - 2026-10-04

### Novidades
- **Processos no Monitor:** ao lado do painel, a aba **Processos** lista as sessões e as consultas em andamento do servidor, com filtros. De lá é possível cancelar uma consulta ou encerrar uma sessão. Disponível em todos os motores que expõem isso: SQL Server, PostgreSQL, MySQL, Oracle, MongoDB, Redis e a maioria dos demais.
- **Autenticação do Windows no SQL Server:** com o usuário atual (SSPI no Windows, Kerberos no macOS e Linux) ou com usuário e senha de domínio, também a partir de Mac e Linux.
- **Kerberos no MongoDB.**

### Melhorias
- No ODBC, os atributos extras da conexão substituem os do modelo.
- O assistente de IA tem seu próprio ícone e não se confunde mais com **Formatar**.
- As consultas feitas pelo assistente são exibidas traduzidas em todos os idiomas.
- A telemetria anônima também conta o uso do assistente, do servidor MCP, das sincronizações, das migrações e das consultas em vários bancos. Nunca nomes, consultas nem dados; é desativada em Configurações › Geral.

## [0.1.5] - 2026-10-03

### Novidades
- **O assistente de IA lê o seu banco, com a sua aprovação:** com um modelo local, pode consultar a estrutura e o uso de índices da conexão (por exemplo, "analise os índices e me diga qual está sobrando"). Antes de ler linhas ou executar uma consulta, mostra o SQL exato e o banco, com Aprovar ou Rejeitar. Nunca altera dados nem estrutura.

### Melhorias
- **Parar** interrompe a resposta do assistente a qualquer momento e **Nova conversa** está sempre disponível.
- A opção "estrutura" do chat não é mais necessária: o assistente pede os detalhes quando precisa deles.
- O cursor de texto aparece onde é possível selecionar ou digitar.

## [0.1.4] - 2026-10-03

### Novidades
- **Projetos:** repositórios Git de SQL vinculados às suas conexões, a partir do segundo ícone da barra lateral. Árvore de arquivos, banco ativo ou ambientes (dev/qa/prod) sem credenciais no repositório, alterações com diff, commit, pull e push. Cada banco mostra no Explorador os projetos vinculados.
- **Executar uma consulta em vários bancos ao mesmo tempo:** a mesma consulta em vários bancos de uma conexão, com os resultados juntos e uma coluna que indica o banco.
- **O DBine se atualiza sozinho:** baixa a nova versão, verifica sua assinatura e reinicia (pergunta antes se houver tarefas em segundo plano). A 0.1.4 é instalada manualmente pela última vez. No Linux funciona com o AppImage; com .deb/.rpm continua oferecendo o download.
- **Desabilitar e habilitar índices** a partir do explorador e da aba Índices, nos motores que permitem (SQL Server, MySQL, MariaDB, TiDB, Oracle, Firebird, CockroachDB, MongoDB…).
- **Seleção de células na grade:** um bloco (arrastando, Shift+clique ou Shift+setas) para copiá-lo, ou células e linhas alternadas com Cmd/Ctrl+clique.
- **Comparação de esquemas:** pode excluir um elemento à esquerda, à direita ou em ambos os lados e, antes de executar, mostra o que depende dele.

### Melhorias
- **Assistente de IA:** recomenda um modelo integrado maior conforme a memória do seu computador, conhece as particularidades de cada dialeto, tenta de novo se se recusar a responder e guarda o histórico de conversas em um painel.
- O painel de Tarefas tem **Remover concluídas** no topo e fecha com Escape ou com um clique fora.
- Azure SQL Database (também Hyperscale): conectado ao master, lista todos os bancos do servidor.
- CockroachDB: os índices aparecem como BTREE e GIN, igual ao PostgreSQL.
- O texto do chat de IA pode ser selecionado e copiado.

### Correções
- A barra da consulta não se desmonta mais ao abrir o painel de IA.
- A sincronização de esquemas do libSQL não falha mais por causa de uma instrução `PRAGMA` que o servidor rejeita.

## [0.1.3] - 2026-10-02

### Novidades
- **Várias janelas** na mesma instância: **Nova janela** a partir do Dock, da barra de tarefas, Arquivo › Nova janela ou Cmd/Ctrl+Shift+N. Conexões, consultas salvas e configurações são compartilhadas entre as janelas.
- **Tarefas em segundo plano:** as operações longas (sincronizar esquemas ou dados, backups, gerar scripts, importar, exportar, clonar tabelas, excluir objetos) podem continuar em segundo plano. O painel de Tarefas mostra progresso, tempo decorrido, tempo restante estimado e Cancelar, e avisa ao terminar. Ao fechar o aplicativo com tarefas em andamento, pede confirmação antes de cancelá-las.
- **Uso de índices** em todos os motores que o informam: chaves PK e FK nas colunas, pasta Índices, porcentagem de leituras por índice com cor conforme seeks e scans, e exclusão de um índice a partir do explorador.
- **Ver dependências…:** o que depende de uma tabela, coluna, view ou rotina.

### Melhorias
- A sincronização de dados aplica cada lado em uma única transação.
- Autocompletar de SQL depois de "esquema." e "tabela.".
- A comparação de esquemas sincroniza comentários, tem setas reversíveis e lista redimensionável.

### Correções
- A sincronização de esquemas exclui as chaves estrangeiras duplicadas uma a uma e, no SQL Server, altera com segurança o índice clustered de uma tabela.

## [0.1.2] - 2026-10-01

### Novidades
- **Execução de scripts como na ferramenta de cada motor:** instrução por instrução, com `GO` / `GO N`, `DELIMITER`, `/` e `SET TERM`. Opção **Continuar em caso de erro**, mensagens ao vivo em ordem, erros com código e linha, e execução da instrução no cursor.
- **Transações Auto/Manual** com Confirmar e Desfazer, e confirmação antes de um UPDATE ou DELETE sem WHERE.
- **Esquemas:** criar e excluir esquemas com proprietário e permissões; os esquemas vazios aparecem no explorador.
- **Aviso de nova versão:** o DBine avisa quando há uma nova versão, ao abrir e em Ajuda › Verificar atualizações….
- Reordenar conexões e pastas arrastando.
- Excluir linhas na grade de dados e salvar as alterações com Cmd/Ctrl+S.

### Melhorias
- Cancelar uma consulta mantém a sessão.

### Correções
- O driver do Solr foi republicado (compartilha código com o do Elasticsearch).

## [0.1.1] - 2026-09-30

### Novidades
- **Telemetria anônima**, ativada por padrão, com um aviso na primeira vez. É desativada em Configurações ou com `DO_NOT_TRACK` / `DBINE_TELEMETRY=0`.
- PostgreSQL: opções de identidade (colunas identity) ao projetar tabelas.

### Melhorias
- Oracle: as definições incluem os índices.
- Aba de definição completa, com mensagens de erro de migração mais claras.
- A comparação de esquemas mantém as linhas e sincroniza em um único passo.
- A aba de comparação de dados lembra suas seleções.
- Drivers do PostgreSQL e do Oracle atualizados para a 0.1.2.

## [0.1.0] - 2026-09-30

### Novidades
- Primeira versão do DBine, com instaladores para Windows, macOS (Apple Silicon e Intel) e Linux. Os drivers de cada motor, exceto o SQLite, são baixados na primeira vez que você se conecta.
