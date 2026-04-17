# Plan: Self-Hosted ADS Router (TCP + MQTT) as Additive MCP Toolset

## Context

Heute: Der MCP-Server nutzt `Beckhoff.TwinCAT.Ads` (NuGet) und braucht deshalb einen **lokal installierten AMS Router** (System-Router auf `127.0.0.1:48898`). Auf Hosts ohne TwinCAT-Installation schlägt jeglicher ADS-Zugriff fehl — weder TCP noch ADS-over-MQTT funktionieren.

Ziel: Der Prozess soll seinen **eigenen AMS Router in-process** hochfahren können (TCP und/oder MQTT), Routen selbst verwalten und auf dem Remote-Target (SPS) sich selbst als Route registrieren, damit Antworten zurückfließen. **Keine Änderung an `AdsService`, `AdsConnectionContext`, `AdsClientAdapter` oder bestehenden `RouteTools` nötig** — die bestehenden ADS-Calls funktionieren unverändert, sobald der in-process Router gestartet ist (weil `AdsClient` ohnehin über `AmsConfiguration.RouterLoopbackEndPoint` auf `127.0.0.1:48898` geht).

Ergebnis: Neues Tool-Paket `AdsRouterTools` (MCP) + neuer `AdsRouterService` (singleton, Lifecycle-gesteuert) + NuGet-Referenz auf `Beckhoff.TwinCAT.Ads.TcpRouter`. Bestehende Dateien bleiben inhaltlich unberührt; nur `ServiceCollectionExtensions.cs` bekommt eine zusätzliche Registrierung (reiner Aggregator, kein Behavior-Change).

## Package-Änderungen

- **Hinzufügen** in `Avm.Swiss.TwinCAT.CLI.csproj`:
  - `<PackageReference Include="Beckhoff.TwinCAT.Ads.TcpRouter" Version="7.0.132" />`
  - Dieses Paket enthält `TwinCAT.Ads.TcpRouter.AmsTcpIpRouter` (TCP In-Process Router) und zusammen mit dem bereits referenzierten `Beckhoff.TwinCAT.Ads.ConfigurationProviders` (hat `MqttConfig`, `RouteConfig`, `TlsConfig`) auch die MQTT-Betriebsart.
- Keine Änderung an bestehenden Referenzen: `Beckhoff.TwinCAT.Ads`, `.Abstractions`, `.Server`, `.ConfigurationProviders` bleiben auf `7.0.132`.

## Neue Dateien

Alle unter `Avm.Swiss.TwinCAT.CLI/src/…` — additive only.

### `src/Services/AdsRouter/AdsRouterOptions.cs`
POCO für Konfiguration: `Mode` (Enum: `Tcp | Mqtt`), `LocalAmsNetId` (string, frei wählbar, Beispiel `"1.1.1.1.1.1"`), `TcpPort` (default `0xBF02`), `BrokerAddress`, `BrokerPort`, `MqttTopic`, `MqttUser`, `MqttPassword`, `UseTls`, `AutoStart` (bool — falls `true` wird Router beim Service-Startup hochgefahren, aus `appsettings.json` gelesen).

### `src/Services/AdsRouter/IAdsRouterService.cs`
Interface:
```csharp
Task<Result<RouterStatus>> StartAsync(AdsRouterOptions opt, CancellationToken ct);
Task<Result<bool>>         StopAsync(CancellationToken ct);
RouterStatus               GetStatus();
Result<bool>               AddLocalRoute(LocalRouteSpec spec);
Result<bool>               RemoveLocalRoute(string nameOrNetId);
IReadOnlyList<LocalRouteSpec> ListLocalRoutes();
Task<Result<bool>>         AddRemoteRouteAsync(RemoteAddRouteRequest req, CancellationToken ct);
```

### `src/Services/AdsRouter/AdsRouterService.cs`
Singleton, kapselt eine Instanz `AmsTcpIpRouter` (TCP-Mode) oder `VirtualAmsNetwork` + Broker-Client (MQTT-Mode). Verantwortlich für:
- Vor erstem Start: `AmsConfiguration.RouterLoopbackEndPoint` setzen, sodass der bestehende `AdsClient` über unseren in-process Router routet (Default `127.0.0.1:0xBF02` — ok, solange kein System-Router konkurriert; bei Konflikt: alternativer Port + Doku-Hinweis).
- `StartAsync`: Router-Instanz erzeugen, statisch konfigurierte Routen (aus Options) hinzufügen, `router.StartAsync(cts.Token)` in Background-Task, State umschalten.
- `StopAsync`: CancellationTokenSource cancel, Router disposen, State zurücksetzen.
- `AddLocalRoute`/`RemoveLocalRoute`: über `AmsTcpIpRouter.AddRoute(new Route(...))` bzw. `RouteCollection.Remove(...)`.
- Kollisionsprüfung: wenn Port `0xBF02` belegt (System-Router läuft), Fehler mit klarer Meldung zurückgeben (`Port 48898 in use — disable the Beckhoff system router or choose a different TcpPort`).

### `src/Services/AdsRouter/AdsRouterRemoteAddRouteClient.cs`
Implementiert den UDP-Discovery-Teil (Port `0xBF03` / 48899): sendet einen `AddRoute`-Broadcast bzw. Unicast an das PLC-Target, Payload enthält Selbst-AmsNetId, IP/Hostname und — falls nötig — Target-Credentials (NT-Username/Password, SHA1 oder Klartext je nach Target-Version). Damit wird unsere NetId auf dem Target als statische Route eingetragen, ohne TwinCAT Engineering.
- Unabhängig vom Router-Mode nutzbar (auch wenn noch kein Router läuft).
- Keine Abhängigkeit von `RuntimeService`/COM/DTE — reine UDP-Socket-Logik.

### `src/Mcp/Models/AdsRouterResults.cs`
Records: `RouterStatus` (Mode, Running, LocalNetId, Endpoint, RouteCount, Connections), `LocalRouteSpec` (Name, NetId, Address, Transport), `RemoteAddRouteRequest` (TargetAddress, TargetNetId, SelfNetId, Username, Password, RouteName, NoEncryption), `AdsRouterStartResult`, `AdsRouterOperationResult`, `AdsRouterRouteListResult`, …  
Wrap-Muster wie bestehend: `McpResult<T>.ToCompactResponse()`.

### `src/Mcp/Tools/AdsRouterTools.cs`
Neue Klasse `[McpServerToolType] public class AdsRouterTools`. DI-injected: `IAdsRouterService`, `ILogger`. Tools:

| Tool-Name | Parameter | Zweck |
|-----------|-----------|-------|
| `adsRouterStart` | mode (tcp/mqtt), localNetId, tcpPort?, brokerAddress?, brokerPort?, topic?, user?, pass?, tls? | Startet in-process Router. Idempotent — wenn bereits läuft, gibt aktuellen Status zurück. |
| `adsRouterStop` | — | Stoppt Router sauber. |
| `adsRouterStatus` | — | Mode, Endpoint, laufende Verbindungen, Routenzahl. |
| `adsRouterAddLocalRoute` | name, netId, address, transport (tcp/mqtt) | Route im in-process Router anlegen. |
| `adsRouterRemoveLocalRoute` | nameOrNetId | Route entfernen. |
| `adsRouterListLocalRoutes` | — | Alle Routen auflisten. |
| `adsRouterAddRemoteRoute` | targetAddress, targetNetId, selfNetId, routeName, username, password, noEncryption=true | UDP `AddRoute` auf Target — registriert uns dort. Unabhängig vom Router-Start. |

Tools **prüfen nicht** `IMcpSessionManager.IsConnected` (DTE) — das ist der große Unterschied zu den existierenden `RouteTools`. Sie arbeiten völlig losgelöst vom TwinCAT-Engineering-Stack.

## Änderungen an bestehenden Dateien (minimal, additiv)

### `Avm.Swiss.TwinCAT.CLI.csproj`
- Eine neue `<PackageReference>`-Zeile (siehe oben).

### `src/Extensions/ServiceCollectionExtensions.cs`
- In `AddMcpServices()` **eine** neue Registrierung anhängen:
  ```csharp
  services.AddSingleton<AdsRouterService>();
  services.AddSingleton<IAdsRouterService>(sp => sp.GetRequiredService<AdsRouterService>());
  services.AddSingleton<AdsRouterTools>();
  ```
- Optional: `AddAdsRouterServices()`-Extension-Methode in separater File statt im bestehenden Aggregator. Entscheidung: im Aggregator belassen, um User-Facing-Oberfläche (`AddMcpServices`) stabil zu halten — der Zusatz ist 3 Zeilen und ändert kein Verhalten existierender Tools.

### `appsettings.json`
Neuer optionaler Block — wird nur gelesen, wenn `AutoStart = true`. Kein Default-AutoStart, um bestehende Setups (Host mit System-Router) nicht zu beeinflussen:
```json
"AdsRouter": {
  "AutoStart": false,
  "Mode": "Tcp",
  "LocalAmsNetId": "1.1.1.1.1.1",
  "TcpPort": 48898,
  "BrokerAddress": null,
  "BrokerPort": 8883,
  "MqttTopic": "VirtualAmsNetwork1",
  "Routes": []
}
```

## Bestehende Dateien, die **nicht** angefasst werden

- `src/Services/AdsService.cs`
- `src/Services/AdsConnectionContext.cs`
- `src/Services/AdsClientAdapter.cs`
- `src/Services/AdsSymbolBrowser.cs`, `AdsVariableAccessor.cs`, `AdsTypeIntrospector.cs`, `AdsRpcInvoker.cs`
- `src/Mcp/Tools/AdsTools.cs`, `AdsWriteTools.cs`, `AdsTraceTools.cs`, `RouteTools.cs`
- `src/Services/AdsTraceService.cs`
- Alle Models unter `src/Models/Ads*.cs`

Diese laufen nach Router-Start unverändert — `AdsClient.Connect(targetNetId, 851)` spricht über `127.0.0.1:48898` mit unserem neuen In-Process-Router, der die Pakete an `targetNetId` weiterroutet.

## Kritische Pfade & relevante Beckhoff-APIs

- `TwinCAT.Ads.TcpRouter.AmsTcpIpRouter` (NuGet `Beckhoff.TwinCAT.Ads.TcpRouter`) — Konstruktor `(AmsNetId localNetId)`, Methoden `StartAsync(CancellationToken)`, `AddRoute(Route)`, `RemoveRoute(...)`, Property `Routes`.
- `TwinCAT.Ads.Configuration.Route` (NuGet `Beckhoff.TwinCAT.Ads.ConfigurationProviders`) — `Route(string name, AmsNetId netId, string address)`.
- `TwinCAT.Ads.Configuration.MqttConfig` / `TlsConfig` / `PskConfig` — für MQTT-Modus.
- `TwinCAT.Ams.AmsConfiguration.RouterLoopbackEndPoint` (Setter, **vor** erster `AdsClient`-Instanziierung aufrufen).
- Für Remote-Route-Add: UDP-Broadcast auf `255.255.255.255:48899`, Frame-Format dokumentiert in Beckhoff InfoSys (ADS/AMS Spec, `AddRoute` Command). Kein offizielles C#-API — selbst geschriebener UDP-Client nötig (~100 Zeilen, bekanntes Frameformat, Implementierungen in `pyads` / `ads-client` als Referenz).

## Implementierungsreihenfolge

1. Package hinzufügen + `dotnet restore` grün.
2. `AdsRouterOptions`, `IAdsRouterService`, `AdsRouterService` (TCP-Mode only) + DI-Registrierung → Unit-Test: Service starten, Status abfragen, stoppen.
3. `AdsRouterTools` (Start/Stop/Status/AddLocal/RemoveLocal/List) → via MCP-Tool-Call testen.
4. MQTT-Mode in `AdsRouterService`.
5. `AdsRouterRemoteAddRouteClient` (UDP AddRoute).
6. `adsRouterAddRemoteRoute` Tool.

Jeder Schritt für sich auslieferbar.

## Verification

### Unit-Tests (neu, `Avm.Swiss.TwinCAT.CLI.Tests/`)
- `AdsRouterServiceTests.cs`: Start/Stop Idempotenz, Port-Konflikt-Fehlermeldung, AddLocalRoute reflektiert in `ListLocalRoutes()`.
- Test-Setup analog zu `AdsServiceIntegrationTests.cs` (direkte Konstruktion, `[Collection("Sequential")]`).

### Integration-Test (manuell, mit echter SPS)
1. `adsRouterStart` mit `mode=tcp`, `localNetId=1.1.1.1.1.1`.
2. `adsRouterAddLocalRoute` für PLC: name=`PLC01`, netId=`<SPS-NetId>`, address=`<SPS-IP>`.
3. `adsRouterAddRemoteRoute` mit PLC-Credentials → Target trägt uns ein.
4. Bestehendes `adsConnect` mit `<SPS-NetId>:851` → muss ohne System-Router Erfolg melden.
5. `adsBrowse` / `adsRead` → echte Symbole/Werte.
6. `adsRouterStop` → `adsRead` danach wirft "not connected" (erwartet).

### MQTT-Integration (manuell)
1. Lokalen Mosquitto-Broker auf `localhost:1883`.
2. Auf Target-SPS in TwinCAT-Engineering einen MQTT-Router-Eintrag anlegen, der denselben Broker/Topic nutzt.
3. `adsRouterStart mode=mqtt brokerAddress=localhost brokerPort=1883 topic=VirtualAmsNetwork1`.
4. `adsConnect` → `adsRead` — Pakete gehen über MQTT-Broker.

### Rauchtest bestehendes Verhalten
- Tests unter `AdsServiceIntegrationTests` und `AdsServiceSymbolCacheTtlTests` laufen weiterhin grün (Router nicht gestartet → alter Pfad über System-Router unverändert).

## Offene Punkte (Entscheidung vor Implementation)

1. **Soll der In-Process-Router auch gestartet werden, wenn ein System-Router bereits auf `:48898` läuft?** Vorschlag: Nein — klare Fehlermeldung mit Hinweis auf alternativen `TcpPort` und `AmsConfiguration.RouterLoopbackEndPoint`-Override. Alternativ könnte der Service das dynamisch erkennen und auf Port `48899`/`48997` ausweichen.
2. **Persistenz der Routen**: Sollen Routen, die via `adsRouterAddLocalRoute` hinzugefügt werden, in `appsettings.json` / separater `StaticRoutes.xml` gespeichert werden, damit sie beim nächsten Start wieder da sind? Vorschlag: Phase 2 — zunächst nur in-memory.
3. **`AdsRouterRemoteAddRouteClient`**: Reicht Broadcast für typische Office-Netze aus, oder sollten wir Unicast-Variante als Default anbieten? Vorschlag: Unicast-Default mit Broadcast-Fallback via Flag.
