# Stock firmware

Reolink's firmware is not in this repository. Download TrackMix PoE firmware
v3.0.0.5428_2604282150 from the
[Reolink Download Center](https://reolink.com/download-center/) and put the
files in `stock/upgrade/`:

| File | sha256 |
|---|---|
| `Reolink-TrackMix-PoE.5428_2604282150.IPC_529SD78MP.paks` | `bda98ea88766e8087d6e1f2454b6783a171305e0f7142e7df7d2f6aaee5b7131` |
| `descriptor` | `617fd1909b1fd66d3f7027c2d920e4ef9ca38bcf4349b7e769abed2f58c2b7c7` |

The build refuses any other `.paks`. `stock/upgrade/` is also the SD card
recovery set: copy it to a FAT card as `/upgrade`. The `descriptor` is only
needed for SD card recovery; its content is:

    dst_ver="Reolink-TrackMix-PoE.5428_2604282150.IPC_529SD78MP.paks"
    board="IPC_529SD78MP"
    model="Reolink TrackMix PoE"
    gpio_grp="PGPIO"
    gpio_pin="21"
    on_lvl="1"
