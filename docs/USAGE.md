# Como usar o NP2PTP

Guia rápido, só o essencial pra compartilhar e baixar arquivos. Pra entender como
funciona por dentro, veja o [README](../README.md).

## 1. Pegue o binário

[Releases](https://github.com/LuanBogoqb/np2ptp/releases/latest) — baixe
`np2ptp-windows-x86_64.exe` (Windows) ou `np2ptp-linux-x86_64` (Linux). Não precisa
instalar nada, é um binário só.

## 2. Criar um `.nptp` (empacotar o que você quer compartilhar)

Funciona com **um arquivo único ou uma pasta inteira** (mantém a estrutura de
subpastas):

```sh
np2ptp pack meuarquivo.zip --out meuarquivo.nptp

# ou uma pasta inteira:
np2ptp pack ./minha-pasta --out minha-pasta.nptp
```

O `.nptp` gerado é pequeno — só metadados (hashes), não o conteúdo. É esse arquivo
que você manda pra quem vai baixar (e-mail, Discord, etc.); o conteúdo de verdade
continua só na sua máquina, guardado numa pasta store (`.np2ptp-store` por padrão).

## 3. Deixar disponível pra rede

O `.nptp` sozinho não basta — alguém também precisa conseguir se conectar em você.
Roda:

```sh
np2ptp serve meuarquivo.nptp
```

e deixa essa janela aberta enquanto quiser compartilhar (igual "seedar" um torrent).
Funciona mesmo atrás de CGNAT / sem porta aberta no roteador — o programa detecta
isso sozinho e usa um relay público automaticamente, sem precisar configurar nada.

## 4. Baixar um `.nptp`

```sh
np2ptp fetch meuarquivo.nptp --out ./baixado
```

Se você só tem o link (`np2ptp:abc123...`), sem o arquivo `.nptp` em mãos, funciona
igual:

```sh
np2ptp fetch np2ptp:abc123... --out ./baixado
```

Ele acha sozinho quem está servindo aquele conteúdo, baixa peça por peça, e confere
a integridade de cada uma antes de gravar — não tem como chegar corrompido ou
adulterado sem ser detectado.

## Pastas vs. arquivo único

- Arquivo único → `--out` é o caminho do arquivo restaurado.
- Pasta → `--out` é o diretório de destino; a estrutura de subpastas é recriada
  dentro dele.
- Arquivos repetidos dentro da pasta (ou entre pacotes diferentes) só são
  transferidos uma vez — dedup automático.

## Extras rápidos

- `np2ptp info meuarquivo.nptp` — lista o que tem dentro de um `.nptp` sem baixar nada.
- `np2ptp fetch ... --fec` — baixa por códigos de correção de erro (RaptorQ) em vez
  de pedaço-a-pedaço; útil se os seeders forem entrando e saindo.
