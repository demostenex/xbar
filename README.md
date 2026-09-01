# xbar

Barra modular para X11, inicialmente orientada a i3.

## Arquitetura atual

Sensores X11/RandR e i3 IPC produzem eventos de domínio. O reducer atualiza o
`State`; mudanças marcam a interface como suja e o renderer X11 redesenha as
dock windows necessárias. O processo bloqueia em `poll(2)` quando não há eventos.

## Milestone 1

O M1 cria uma janela dock por output RandR ativo, reserva espaço com EWMH,
mostra workspaces e reage a eventos de workspace, foco e output do i3. O estado
também preserva explicitamente o XID da janela focada.

No M1 não havia DBus. Global Menu/DBusMenu, tray, notificações, métricas,
transparência, blur, EGL, OpenGL e Camaleão continuam fora do escopo.

## Milestone 2A

O M2A adiciona somente o serviço `com.canonical.AppMenu.Registrar` e seu
registry de associações XID → endpoint DBus. O serviço captura o sender da
chamada `RegisterWindow`, suporta unregister explícito e remove registros
quando um unique name desaparece. DBusMenu, menus visuais e ações ainda não
fazem parte deste incremento.

O adapter DBus roda em uma thread própria com zbus/async-io. Eventos chegam ao
loop principal por um socket local usado como wakeup; o loop continua usando
`poll(2)` e não faz polling periódico do DBus.

## Milestone 2B

O M2B adiciona ao worker DBus a leitura assíncrona de `com.canonical.dbusmenu`
via `GetLayout`, convertendo o payload para `MenuModel` antes de emitir
`MenuLoaded`. O carregamento é disparado quando o foco ou o registro do menu
muda, e `LayoutUpdated`/`ItemsPropertiesUpdated` invalidam o snapshot para um
novo carregamento completo. O estado distingue `NoMenu`, `Loading`, `Loaded` e
`Error`; respostas carregadas carregam janela, endpoint e request id, portanto
respostas antigas são descartadas pelo reducer.

O fixture publica a árvore controlada `Arquivo → Novo/Sair`, `Editar` e `Ajuda`
em `com.canonical.dbusmenu` no caminho `/com/xbar/FixtureMenu`, incluindo
atalho, ícone, item desabilitado e item oculto. Nenhum menu é renderizado na
barra neste milestone.

## Compilação e execução

```bash
cargo build
cargo test
I3SOCK=/caminho/para/ipc-socket cargo run
```

O fixture controlado do Registrar pode ser executado com:

```bash
cargo run --bin appmenu-fixture -- --xid 0x1234 --query
```

Ele registra um endpoint de teste, permanece conectado por 30 segundos e
depois sai; `--unregister` testa o caminho explícito.

O socket também pode ser descoberto pela propriedade X11 `I3_SOCKET_PATH`.
É necessário executar dentro de uma sessão X11 com i3 e RandR disponíveis.

## Roadmap

1. Fundação X11/i3
2. Global Menu (AppMenu Registrar + DBusMenu)
3. Renderer e interação
4. System Tray
5. Notificações
6. Métricas
7. Blur via propriedade consumida pelo xomposite
8. Camaleão
