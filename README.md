# TokenMiner · minero nativo

Programa de minería para Windows y tarjetas NVIDIA. Mina Pearl y Quantus, y **sólo mientras
resulta rentable**: compara cada hora lo que produce la gráfica con lo que cuesta esa
electricidad, y para cuando deja de compensar.

Es el programa que se instala desde [tokenminer.app](https://tokenminer.app). Este repositorio
contiene su código completo para que cualquiera pueda comprobar qué hace antes de ejecutarlo.

## Qué hace

- **Mina sólo cuando sale a cuenta.** En España lee el PVPC hora a hora de Red Eléctrica, con IVA
  e impuesto eléctrico incluidos; en cualquier otro sitio, con un precio fijo por kWh. Lo que paga
  cada moneda lo lee de Kryptex y el cambio del euro, del Banco Central Europeo.
- **Elige la moneda más rentable.** Mide Pearl y Quantus en el propio equipo y se queda con la que
  más deja después de la luz. Para no saltar por ruido del mercado, la otra debe ganar por más de
  un 5 % durante media hora.
- **Deja sitio a los juegos.** Lee cuánta GPU usan los demás programas con los contadores de
  Windows y reparte el tiempo para no pasar del límite elegido, de 0 a 100 %.
- **Varias gráficas.** Un motor por GPU, con sus propias medidas.
- **No toca los ajustes de la tarjeta.** Ni voltajes, ni curvas, ni frecuencias: sólo lee uso y
  consumo.
- **Funciona con la web cerrada.** Se queda en la bandeja del sistema, con horario y arranque
  opcional con el equipo.

## Requisitos

- Windows 10 u 11.
- Tarjeta NVIDIA. Quantus funciona desde la GTX 900 (Maxwell) y Pearl desde la RTX 30. En RTX 50
  los dos usan kernels ajustados; en el resto, los genéricos compilados desde las mismas fuentes.
- Rust estable para compilarlo.

## Compilar

```powershell
powershell -File build.ps1 -Origin https://tokenminer.app
```

`-Origin` son los orígenes web que podrán hablar con el programa, separados por comas, y quedan
**compilados dentro del ejecutable**: cambiar de dominio exige volver a compilar. Opcionalmente,
`-Comision` y `-CuentaComision` fijan el reparto por turnos descrito abajo.

Genera `dist/tokenminer-setup.exe` y un manifiesto con versión, orígenes y SHA256.

Los motores de minería van en crates aparte, cada uno con sus kernels, sus fuentes y sus pruebas:
`tokenminer-quantus` y `tokenminer-pearl`. `Cargo.toml` indica de dónde los toma.

## Cómo habla con la web

El programa escucha en `127.0.0.1:39847`. Sólo acepta conexiones cuyo `Origin` coincida
exactamente con uno de los fijados al compilar. La web manda `start`, `stop`, `settings` y `quit`; el
programa responde con su estado, las shares aceptadas, la velocidad de cada gráfica y las cuentas
de rentabilidad.

El navegador pedirá permiso para alcanzar el equipo la primera vez.

## Comisión

Quien cobra directamente en su cartera cede una parte del tiempo de minado a la cuenta de
TokenMiner, mediante turnos. El porcentaje se fija al compilar y el programa lo publica en su
estado, de modo que la web puede enseñarlo. **El cálculo de rentabilidad descuenta esa parte**: la
decisión de minar o parar se toma con lo que se lleva el usuario, no con el total. Quien elige
cobrar en créditos de IA no paga comisión.

Está en `src/pool.rs` y `src/residente.rs`, y hay pruebas que lo comprueban.

## Pruebas

```powershell
cargo test
```

Las que necesitan una GPU libre o salen a internet están marcadas `#[ignore]` y se lanzan aparte:

```powershell
cargo test --release -- --ignored --test-threads=1
```

Incluyen el contraste de los candidatos de la GPU contra la verificación completa en CPU.

## Kernels

Los kernels están en los crates de cada motor, junto con sus fuentes CUDA y un `manifest.json` que
registra, para cada binario, de qué fuente sale, con qué opciones se compiló y los SHA256 de la
fuente y de la imagen. Así se puede recompilar y comprobar que coinciden.

## Licencia

GPL-3.0-only. Puedes leer, modificar y redistribuir el código; si distribuyes una versión
modificada, tienes que publicar la tuya bajo la misma licencia.

Los motores incorporan código de terceros bajo sus propias licencias: el de Quantus, crates del
minero oficial de Quantus Network (Apache-2.0) y `qp-poseidon-core` (MIT); el de Pearl, código de
Pearl Research Labs (ISC) y `plonky2` (MIT o Apache-2.0). Detalle en
[THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt) y en `licenses/`.
