# Alterações

## [1.0.0] - 2026-10-19

### Novidades
- **Tema claro:** um tema claro e uma opção **Igual ao sistema** (a padrão) que acompanha o seu sistema operacional quando ele muda; zoom da interface com Cmd/Ctrl + "+", "−", "0" ou a roda do mouse, e tamanhos de fonte separados para o editor e para a grade de resultados, em **Configurações › Geral**.
- **Modo Vim e seus próprios atalhos:** modo Vim para o editor SQL (Configurações › Geral), e cada atalho de teclado pode ser alterado em **Configurações › Atalhos de teclado**, com busca, redefinição e avisos de conflito.
- **Editor dividido:** até três grupos de editores lado a lado ou um acima do outro (o menu da aba, o botão da barra de abas ou Cmd/Ctrl+\); arraste abas entre os grupos ou para a metade direita ou inferior de um grupo para dividi-lo. O layout é restaurado ao reiniciar.
- **Login na nuvem para bancos gerenciados:** as conexões PostgreSQL e MySQL com Amazon RDS e Aurora, Google Cloud SQL e AlloyDB, e Azure Database podem fazer login com **AWS IAM**, **Google Cloud IAM** ou **Microsoft Entra ID**. O DBine gera um token novo a partir do seu perfil da AWS (SSO incluído), do gcloud ou do az antes de cada conexão, então acabaram os tokens para colar que expiravam em 15 minutos, e **Testar** mostra com qual identidade o login foi feito.
- **Senhas a partir de um comando ou de um gerenciador de segredos:** qualquer campo secreto de qualquer motor, e a senha e a frase secreta da chave do túnel SSH, podem obter o valor de um comando, 1Password, HashiCorp Vault, AWS Secrets Manager, Azure Key Vault ou Google Secret Manager. Ele é obtido a cada conexão e mantido somente na memória, com uma opção de chave JSON e um teste por campo que mostra apenas o tamanho. Um comando ou gerenciador de segredos que veio de uma importação ou de um backup restaurado só é executado depois que você o confirmar neste computador.
- **KeePass e Zoho Vault como fontes de senhas:** qualquer campo secreto (e o do túnel SSH) pode obter o valor de uma entrada de um banco KeePass (.kdbx, lido pelo próprio DBine; senha mestra vinda do chaveiro, pedida ao conectar ou obtida de um comando; arquivo de chave opcional) ou de um segredo do Zoho Vault (qualquer data center, descriptografado no seu computador com a sua frase secreta mestra), e, se quiser, o usuário da conexão vem da mesma entrada.
- **Credenciais de banco de dados do Vault, CyberArk e Delinea como fontes de senhas:** as senhas das conexões podem vir do mecanismo de segredos de banco de dados do HashiCorp Vault (credenciais geradas para a conexão, renovadas enquanto ela está aberta e revogadas quando ela é fechada; login AppRole também para o Vault), do CyberArk Conjur, do Central Credential Provider do CyberArk (com certificados de cliente) e do Delinea Secret Server.
- **Bitwarden, Bitwarden Secrets Manager e AWS Parameter Store como fontes de senhas:** qualquer campo secreto (e o do túnel SSH) pode obter o valor de um item do Bitwarden, pessoal ou de uma organização, em bitwarden.com, bitwarden.eu ou no seu próprio servidor, descriptografado no seu computador com a sua senha mestra; de um segredo do Bitwarden Secrets Manager (com o token de acesso de uma conta de máquina); ou de um parâmetro do AWS Systems Manager (SecureString, String ou um elemento de uma StringList), com uma chave JSON e, se quiser, o usuário da conexão vindo do mesmo item.
- **Primeiro as credenciais, e explore o seu gerenciador de segredos:** o formulário de conexão pede primeiro as **Credenciais**: guardar a senha no chaveiro, pedi-la a cada vez, ou obter o usuário e a senha de uma linha de comando ou de um gerenciador de segredos, cujos campos os substituem. Um botão **Examinar** lista as entradas de um gerenciador e os nomes dos campos para que você os escolha em vez de digitar as referências; ele mostra apenas nomes, nunca valores.
- **Usuário e senha a partir da entrada:** com KeePass, Bitwarden, 1Password, Zoho Vault, Delinea ou CyberArk CCP como credenciais, tanto o usuário quanto a senha vêm da entrada, e **Configurações avançadas** permite escolher outros campos da lista da própria entrada. Cada fonte pede primeiro o seu login e a entrada por último, e o Examinar fica pronto quando isso está preenchido.
- **Contas do Zoho Vault, conectadas uma única vez:** um assistente guiado **Conectar o Zoho Vault** conduz você pelo console da Zoho, encontra o seu data center e verifica a sua frase secreta mestra; depois, qualquer conexão só escolhe a conta. As contas ficam neste computador e são gerenciadas em **Configurações › Gerenciadores de segredos**.
- **Contas para todos os gerenciadores de segredos:** Bitwarden, Bitwarden Secrets Manager, HashiCorp Vault e OpenBao, Delinea, CyberArk Conjur e Central Credential Provider, 1Password, AWS, Azure Key Vault, Google Secret Manager e KeePass agora se conectam uma única vez em **Configurações › Gerenciadores de segredos** com um assistente guiado que explica onde está cada valor, verifica a conta em tempo real e mostra quem fez login; depois, qualquer conexão só escolhe a conta, e as credenciais vão somente para o servidor do próprio gerenciador.
- **Tenants do Azure para escolher:** o assistente de contas do Azure Key Vault lista os seus tenants do Microsoft Entra (você ainda pode digitar um), e, quando você não está conectado ao tenant escolhido, mostra o comando `az login --tenant` para copiar.
- **Autocompletar em células DataFrame SQL:** as células acima são sugeridas como tabelas (mesmo as que ainda não foram executadas), com as suas colunas depois de `nome.` ou de um alias e na lista do select, e os leitores de arquivos do DuckDB depois de FROM e JOIN; ao escolher um leitor, o cursor fica entre as aspas dele, onde **Escolher arquivo…** abre o diálogo de arquivos. A lista de tabelas depois de FROM não fecha mais sozinha enquanto o notebook é atualizado.
- **Visualização de mapa:** os resultados com colunas de geometria (PostGIS, MySQL, SQL Server, SQLite/SpatiaLite, DuckDB, MongoDB, Elasticsearch e outros) ou colunas de latitude/longitude podem ser exibidos em um mapa, desenhado na sua máquina sobre um mapa-múndi integrado que funciona offline. Os blocos de mapa online com mais detalhes podem ser ativados em **Configurações › Mapas**; o provedor então vê qual região você está olhando, nunca os dados.
- **Oracle no mapa:** as colunas SDO_GEOMETRY agora aparecem no mapa de resultados, tanto nos dados de uma tabela quanto nas suas próprias consultas (convertidas no servidor para WKT com o seu SRID); essas células são somente leitura na grade.
- **Abrir arquivos de dados diretamente:** arquivos SQLite, DuckDB, CSV, TSV, Parquet, JSON e Excel são abertos com um clique duplo ou com «Abrir com o DBine», soltando-os na janela, ou com **Arquivo › Abrir arquivo…** (Cmd/Ctrl+O). Cada um abre como uma conexão temporária que você pode manter com **Salvar conexão**, e os arquivos de dados mostram as primeiras 1.000 linhas na hora.
- **Paleta de comandos:** Cmd/Ctrl+Shift+P encontra e executa qualquer ação, mostrando apenas o que o motor atual suporta, e **Ir para…** (Cmd/Ctrl+P) pula para conexões, bancos, tabelas, views, rotinas, consultas salvas e abas abertas. Cada ação da paleta também pode ganhar um atalho próprio.
- **Pular para linhas relacionadas:** nos dados de uma tabela ou nos resultados de uma consulta, **Ir para a linha referenciada** (ou Cmd/Ctrl+clique) abre a tabela pai filtrada por essa chave estrangeira, e **Linhas que a referenciam** abre cada tabela filha filtrada pelo valor, incluindo chaves compostas. Em todos os motores com chaves estrangeiras.
- **Notebooks SQL:** **Arquivo › Novo notebook** abre um notebook: células SQL que são executadas em qualquer conexão, cada uma mantendo o seu resultado e o seu gráfico abaixo, e células de texto para explicá-las. Ele é salvo como um `.ipynb` padrão do Jupyter que o GitHub, o VS Code e o Jupyter exibem, e que o Azure Data Studio e os notebooks `%%sql` do Jupyter também abrem. O arquivo guarda uma prévia de cada resultado (100 linhas e 1 MB por célula, em **Configurações › Notebooks**) com o total de linhas, a hora e a duração, nunca dados de conexão nem senhas; dentro de um projeto, os notebooks são salvos sem resultados, a menos que você ative. O assistente pode explicar um resultado, propor a próxima célula ou uma versão agrupada de uma consulta grande, sem ver as linhas. Exporta para HTML ou Markdown.
- **Gráficos sobre todo o resultado:** os gráficos são calculados sobre todas as linhas do resultado, não apenas as carregadas na grade. Nos motores SQL o agrupamento é feito no servidor, nas células DataFrame SQL de um notebook quem faz é o DuckDB, e nos demais motores quem faz é o DBine.
- **Mais tipos de gráfico:** histograma, mapa de calor, box plot (com whiskers e outliers), barras com linhas e KPI (o total de todo o resultado).
- **Vários gráficos por célula de notebook:** uma célula de notebook pode mostrar mais de um gráfico do seu resultado, e lembra qual fonte o seu mapa usa.
- **Gráficos nas exportações de notebooks:** os notebooks exportados para HTML ou Markdown incluem os seus gráficos como imagens.
- **Combine resultados de bancos diferentes em um notebook:** o resultado de cada célula é um DataFrame com o nome da célula (exibido e editável no cabeçalho dela), e uma célula **DataFrame SQL** consulta e une os DataFrames acima como tabelas com o SQL do DuckDB e sem conexão, então uma célula pode unir um resultado do PostgreSQL com um do SQL Server e um do MongoDB. Ela é executada em um banco DuckDB em memória que não pode gravar arquivos nem acessar a rede, e é descartado quando o notebook é fechado; as linhas de cada fonte vêm da sua execução nesta sessão ou da sua consulta executada novamente, somente leitura, na própria conexão (até 1.000.000 de linhas por DataFrame, em **Configurações › Notebooks**).
- **Notebooks como relatórios agendados:** uma tarefa agendada pode executar um notebook com o DBine fechado (**Executar notebook**), salvar a cópia executada e exportá-la para HTML ou Markdown, e o próximo passo **Enviar um e-mail** pode anexá-la ou enviar a exportação HTML como corpo do e-mail. Nada é perguntado sem supervisão: ela lê apenas arquivos com **Permitir sempre** para aquele notebook, é executada em ambientes que pedem confirmação somente com a permissão explícita do passo, e as suas células que alteram dados entram na aprovação da tarefa. As exportações agendadas incluem tabelas; os gráficos são incluídos ao exportar pelo aplicativo.
- **Arquivos locais em células DataFrame SQL:** uma célula DataFrame SQL pode ler arquivos CSV, Parquet, JSON ou NDJSON e Excel do seu computador com `read_csv`, `read_parquet`, `read_json` e `read_xlsx`, incluindo padrões como `data/*.csv` e listas de arquivos, arquivos geográficos (GeoJSON, Shapefile com seus arquivos complementares, GeoPackage, KML, GPX, FlatGeobuf) com `st_read`, e tabelas de arquivos de banco SQLite e DuckDB com `sqlite_scan` e `read_duckdb`, cada um como um DataFrame que pode ser unido com os resultados de células de qualquer motor; as geometrias continuam funcionando com funções espaciais como `ST_Contains` e aparecem no mapa. Os arquivos são apenas lidos, nunca gravados. Antes de ler um arquivo pela primeira vez, o notebook pergunta: permitir nesta execução, sempre para este notebook neste computador (um padrão cobre os arquivos novos dele) ou negar; credenciais, chaves, o histórico do shell e arquivos do sistema nunca são lidos. Se o conteúdo de um arquivo mudou desde a última execução, o resultado informa. Configurações › Notebooks lista os arquivos que cada notebook pode ler e permite removê-los. Os mesmos leitores também leem arquivos da web (`https://`) e do Amazon S3, Google Cloud Storage e Azure Blob Storage (`s3://`, `gs://`, `az://`, `abfss://`, com padrões como `s3://bucket/2026/*.parquet`), com a mesma aprovação, que mostra o servidor primeiro; o DBine os baixa por conta própria para uma pasta temporária privada que é removida após a leitura, usando o login da AWS, do Google Cloud ou do Azure deste computador (um perfil da AWS pode ser escolhido em Configurações › Notebooks), e nunca acessa endereços locais nem de redes privadas, nem segue redirecionamentos para outros servidores, nem aceita credenciais na URL. Um GeoPackage aberto em outro programa agora inclui as alterações que ainda estão no seu `-wal`.
- **Exportar um notebook como notebook Python:** **Exportar › Notebook de Python (.ipynb)** grava um notebook do Jupyter que o reproduz: um engine do SQLAlchemy (ou o cliente Python do próprio motor, como pymongo ou redis) por conexão, cada célula de consulta lida em um DataFrame do pandas com o nome da célula, as células DataFrame SQL executadas com o DuckDB sobre esses DataFrames, e as células de texto como Markdown. Senhas e tokens nunca são exportados: o código os lê de variáveis de ambiente, que a primeira célula lista junto com os pacotes a instalar. Os resultados ficam de fora, a menos que você escolha incluir as prévias.
- **Gravar um DataFrame em uma tabela a partir de um notebook:** uma **célula de gravação** (adicionada abaixo de qualquer célula com **Gravar em uma tabela…**) grava todo o resultado de uma célula em uma tabela do SQL Server, PostgreSQL, MySQL, MariaDB, Oracle, SQLite, DuckDB, MongoDB e outros: inserir, atualizar por chave, inserir ou atualizar, ou substituir o conteúdo da tabela, com mapeamento de colunas, prévia e uma tabela nova, se quiser. Cada gravação pergunta antes, mostrando onde, como e quantas linhas (as conexões de produção pedem que você digite o nome da tabela), é executada em uma única transação nos motores que as têm, e as conexões somente leitura nunca são gravadas. Em uma tarefa agendada, uma célula de gravação só é executada quando o passo autoriza aquela célula, tabela e modo.
- **Parâmetros de notebook:** um notebook pode definir parâmetros (texto, número, data, data e hora, booleano ou uma lista fixa, com um valor padrão) que as suas células usam como `{{name}}`; ao executar, os valores que faltam são pedidos, cada valor chega ao banco como dado (vinculado pelo driver ou entre aspas segundo as regras do próprio motor), e cada resultado mostra os valores que o produziram.
- **Mais cinco idiomas:** o DBine e o app para celular agora falam alemão, russo, chinês simplificado, japonês e coreano, além de espanhol, inglês, português, francês e italiano; o idioma acompanha o do seu sistema e só o que está em uso é carregado.

### Melhorias
- **Assinado no macOS:** o aplicativo para macOS é assinado com um Apple Developer ID e notarizado pela Apple, por isso abre sem o aviso de «não foi possível verificar o desenvolvedor» nem qualquer comando de terminal. Na primeira vez que a versão assinada é aberta, o macOS pergunta uma única vez se o DBine pode usar a chave dele no chaveiro: escolha **Permitir sempre**.
- **O login na nuvem só confia no certificado da instância:** as conexões que fazem login com AWS IAM, Google Cloud IAM ou Microsoft Entra, e as conexões do Cloud SQL e do AlloyDB com o arquivo de CA da instância, agora aceitam somente essa CA (ou o bundle do RDS da Amazon), e não qualquer certificado público, antes de enviar um token ou uma senha. Um servidor que pede a senha em texto simples enquanto **Confiar no certificado do servidor** está ativado agora falha em vez de recebê-la.
- **Assinado no Windows:** o instalador do Windows e o aplicativo são assinados com um certificado de assinatura de código, então o Windows mostra quem publica o DBine em vez de «Editor desconhecido».

### Correções
- **Monitor do servidor:** seus números, tabelas e notas estão traduzidos em todos os motores, em vez de mostrar alguns rótulos em espanhol.
- **Assistente de IA e otimizador:** as instruções para o modelo seguem o idioma da interface, então um modelo local responde nesse idioma em vez de em espanhol. A linha que informa o que foi enviado ao modelo também está traduzida.

## [0.1.14] - 2026-10-17

### Novidades
- **DBine no seu celular (Android):** em **Configurações › Dispositivos**, ative **Permitir celulares** e vincule o aplicativo do DBine no celular escaneando um código QR; depois confirme no computador que as duas telas mostram o mesmo código. Pelo celular você vê o Monitor de cada conexão, quem está bloqueando quem, as sessões em andamento e a verificação de saúde de um banco de dados; em **Atividade**, as consultas que você executa no editor do DBine e as tarefas agendadas, com seus detalhes. Se você permitir esse celular, encerrar uma sessão, cancelar uma consulta, ou executar ou cancelar uma tarefa exige sempre a sua digital, o rosto ou o PIN. O celular avisa quando uma consulta longa termina e quando uma tarefa falha ou tem um alerta, também com o aplicativo fechado (a cada 15 minutos). Suas conexões e senhas não saem do computador: o celular pergunta ao DBine por um canal criptografado de ponta a ponta.
- **Acesso do celular de fora da sua rede:** desativado por padrão (somente rede local). Ative em **Configurações › Dispositivos › Fora da rede local**, por meio do seu próprio servidor de retransmissão ou dos servidores públicos do iroh, que veem endereços IP, mas nunca o conteúdo. Como está protegido: [dbine.com/pt/phone-security](https://dbine.com/pt/phone-security).
- **Consultas longas mantêm o computador ativo:** enquanto uma consulta do editor está em execução há 30 segundos ou mais, o DBine impede que o computador entre em modo de suspensão, para que a conexão não caia.
- **Download do aplicativo do celular:** o aplicativo do celular (Android) é publicado a cada versão e no dbine.com.

## [0.1.13] - 2026-10-16

### Melhorias
- **Apoiar o projeto:** o botão de coração (**Apoiar o projeto**), seu lembrete e **Ajuda › Apoiar o projeto** abrem o Buy Me a Coffee.
- **Script com dados de tabelas escolhidas:** em **Gerar script do banco**, com **Dados (INSERT)** marcado, você pode manter os dados de todas as tabelas ou escolher quais os recebem, a partir de uma lista com filtro (**Escolher tabelas…**), de modo que o script leve a estrutura de todo o banco e as linhas de apenas algumas tabelas.
- **Menu do banco:** **Exportar banco…** foi removido. Abria o mesmo diálogo que **Gerar script…**, que continua e faz tudo o que fazia (estrutura, dados e salvar em um arquivo).
- **Estatísticas de uso:** quando o servidor de estatísticas não responde ou falha, o DBine tenta de novo por até uma hora em vez de descartar o evento. Nada mais do que é enviado muda, e desativar as estatísticas interrompe qualquer nova tentativa pendente.
- **Download para Flatpak:** cada versão agora inclui um arquivo `.flatpak` para Linux (`x86_64` e `aarch64`), que se instala com `flatpak install --user ./dbine-X.Y.Z-x86_64.flatpak`; o Flatpak baixa sozinho o runtime do GNOME do Flathub. Ele não se atualiza sozinho: instale o arquivo da versão seguinte da mesma forma.

## [0.1.12] - 2026-10-15

### Novidades
- **Flathub:** o DBine foi enviado ao Flathub como `io.addlayer.dbine`. Essa build inclui todos os drivers, o DuckDB e o motor do assistente de IA, então nada é baixado no primeiro uso, e o Flatpak a mantém atualizada no lugar do DBine. Dentro do sandbox, a ação do firewall do Azure explica que não pode usar o Azure CLI e oferece o portal, T-SQL ou um terminal, e as tarefas agendadas são salvas, mas não podem ser registradas no agendador do sistema.
- **winget:** o DBine foi enviado ao winget no Windows como `AddLayer.DBine` (`winget install AddLayer.DBine` assim que for aceito), e cada nova versão chega a ele sozinha. Instalado com o winget, o DBine continua se atualizando sozinho como antes.

### Melhorias
- **Novo identificador do app:** o DBine agora é `io.addlayer.dbine`. Na primeira inicialização após a atualização, suas conexões, senhas salvas, abas abertas, chats do assistente, configurações e tarefas agendadas são copiadas automaticamente; os dados da versão anterior ficam como estavam, então voltar a ela não perde nada. No Windows e no Linux, um modelo de IA grande pode ser baixado novamente. No macOS, o chaveiro pode pedir acesso uma vez, e algumas permissões de privacidade (como o acesso à rede local) podem ser pedidas de novo; no Windows, um atalho fixado na barra de tarefas com uma versão anterior pode precisar ser fixado novamente.

## [0.1.11] - 2026-10-13

### Novidades
- **Ordenar por coluna:** clique no cabeçalho de uma coluna nos dados de uma tabela para ordenar por ela (de novo para decrescente, uma terceira vez para limpar; Shift+clique adiciona mais colunas). O banco de dados ordena a tabela inteira onde o motor permite, da mesma forma que os filtros; nos outros casos, ou quando o servidor recusa a ordem, as linhas carregadas são ordenadas e um aviso explica o motivo. Os resultados de uma consulta podem ser ordenados da mesma maneira.
- **Controles na barra de status:** em uma aba de consulta, a barra de status azul agora mostra a conexão, o banco de dados, o modo de transação (Auto por padrão, ou Manual) e o limite de linhas. Clique em qualquer um deles para alterá-lo com um seletor rápido, como no VS Code. A barra de ferramentas da consulta não perde mais botões quando a barra lateral está larga: o que não couber vai para um menu **…**.
- **Enviar feedback:** um botão no fim da barra de atividades abre um formulário curto para relatar um erro, sugerir uma melhoria, pedir um novo motor ou fazer uma pergunta. O GitHub abre com a issue já preenchida e com os rótulos, então você só precisa clicar em «Create». O diagnóstico opcional (versão do DBine, sistema operacional, idioma, e o motor e a versão do servidor da conexão ativa) é mostrado antes de ser incluído, e nunca leva servidores, usuários, nomes de bancos de dados nem consultas.

### Melhorias
- **Texto longo na grade:** ao parar sobre uma célula cujo texto não cabe, ele é mostrado inteiro em um tooltip que mantém as quebras de linha e pode ser rolado e selecionado.
- **Quando uma conexão falha:** ao expandir uma conexão, ela primeiro se conecta e mostra **Conectando…**; a árvore só aparece quando o servidor responde (aí instantaneamente, com o que o DBine lembra, enquanto atualiza). Se não conseguir conectar, a conexão mostra um aviso com **Tentar novamente** e **Ver o que houve…**, e as abas (os dados de uma tabela, a execução de uma consulta e as demais ferramentas) mostram o mesmo erro em vez de ficarem em branco. Se a conexão cair depois, volta ao aviso. **Ver o que houve…** explica a falha (firewall, tempo esgotado, recusa, nome do host, certificado ou login) e o que verificar, e no Azure SQL pode adicionar o seu IP ao firewall do servidor com o seu login do Azure CLI, depois de confirmar o servidor, a regra e o IP. Funciona em todos os motores; a causa é mais precisa no SQL Server, Azure SQL, PostgreSQL e MySQL.

### Correções
- **Editor de consultas:** uma aba cuja conexão ainda não está aberta (por exemplo, restaurada ao iniciar) volta a completar nomes de esquemas, tabelas e colunas: ao pedir o autocompletar, a conexão é aberta e a lista aparece quando os nomes chegam. Os snippets não entram mais na lista depois de um ponto.
- **Editor de consultas:** a lista de autocompletar e as dicas abrem para cima quando você digita nas últimas linhas, em vez de ficarem escondidas atrás dos resultados.
- **Firewall do Azure SQL:** **Adicionar meu IP ao firewall do Azure** usa somente o IP público desta máquina, consultado por HTTPS e verificado novamente antes de a regra ser criada. Um IP citado na mensagem de erro de um servidor é mostrado como não verificado e nunca é usado.
- **Tentar novamente após uma conexão com falha:** se, nesse meio-tempo, a aba passou para outra conexão ou outro banco de dados, ao tentar novamente a confirmação de produção é pedida de novo.
- **Renomear no Snowflake e no Trino, Presto e Starburst:** o que nomeia o objeto renomeado (views, views materializadas, funções, procedimentos e, no Snowflake, tarefas, streams e tabelas dinâmicas) é listado para você corrigir manualmente em vez de ser recriado, e assim mantém o seu proprietário e as configurações de segurança. Além disso, a renomeação é recusada quando o código armazenado seria dividido em várias instruções, e reescrever os dependentes não muda mais com qual identidade o código é executado (`DEFINER`, `SQL SECURITY`, `AUTHID`, `EXECUTE AS`).
- **Consultas dos clientes de IA e do assistente:** uma consulta roda sem pedir aprovação somente quando o banco a faz valer como somente leitura e ela chama apenas funções nativas do motor, sem efeitos colaterais. Uma que chame qualquer outra função (uma função de usuário, ou uma que atue fora da transação, como pausar a replicação ou obter um lock) espera antes a sua aprovação.

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
- **Modificar tabela:** os avisos do script que você abre como consulta ficam na própria linha de comentário.
- **Senhas e opções secretas** são mascaradas em todos os lugares onde são editadas, inclusive nas etapas de backup das tarefas agendadas.
- **Ver dependências e Renomear** não travam mais em rotinas com nomes entre aspas incomuns.
- **Somente leitura no SQL Server:** um lote só pode começar com `SELECT`, `WITH`, `USE` ou `PRINT`. O que vem depois de `SHOW`, `DESCRIBE` ou `PRAGMA` é verificado em todos os motores.
- **Importar conexões:** uma URL JDBC do Oracle com usuário e senha mantém esses dados fora do nome da conexão: a senha vai para o chaveiro do sistema.
- **Somente leitura no SQL Server:** um procedimento cujo nome começa como `print_` ou `select1` deixa de ser tomado por uma leitura.
- **Importar conexões:** uma senha do Oracle com `@` é guardada inteira no chaveiro do sistema.
- **Túneis SSH no Linux:** a porta local do túnel só confia em conexões abertas pelo seu próprio usuário.
- **Somente leitura:** uma consulta com um retorno de carro isolado ou um espaço Unicode incomum é recusada, porque os motores discordam sobre onde um comentário ou uma instrução termina ali. Funções com efeitos colaterais também são recusadas: `load_extension` no SQLite, `pg_notify` e leituras de replicação lógica no PostgreSQL, e o cancelamento de sessões ou consultas no Snowflake.
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
- **Exportação SQL:** os valores de texto são escritos do jeito que o motor de origem os lê, então voltam exatos ao executar o script nele. Também não conseguem adicionar instruções em um motor que lê as barras invertidas de outra forma.
  - **PostgreSQL, SQL Server, Oracle, SQLite, DuckDB, Firebird, SAP HANA, Trino, Db2, Teradata, Vertica, Exasol, Netezza e Dremio:** a barra invertida é escrita com a função de caracteres do próprio motor, então nenhuma aparece dentro de um texto. Os scripts do Firebird levam uma linha por `INSERT`, que é só o que o Firebird aceita.
  - **Motores que interpretam escapes com barra invertida** (MySQL, ClickHouse, Snowflake, Redshift…): as barras invertidas e as aspas são escapadas do jeito que esses motores as leem.
  - **Origens desconhecidas** (uma grade de vários bancos de dados) recebem uma forma segura em qualquer motor. A nova opção **Destino com textos padrão** escreve as barras invertidas como estão.
- **Exportação CSV e TSV:** a proteção contra fórmulas também cobre o texto depois de uma `,` ou de um `;`, que uma planilha com outro separador de listas dividiria em sua própria célula.
- **Renomear no Snowflake:** as views, funções e procedimentos que citam o objeto renomeado são listados para você corrigir manualmente, em vez de serem recriados, e assim mantêm o dono, o `EXECUTE AS` e o `SECURE`.

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
