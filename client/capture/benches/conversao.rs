//! Quanto custa a conversão BGRA->NV12, isolada do resto do pipeline.
//!
//! Sem `criterion` de propósito: uma dependência inteira para cronometrar uma
//! função pura não se paga. O que interessa é ordem de grandeza — cabe ou não
//! cabe no orçamento de um frame.

use std::time::Instant;
use tela_capture::video::{ajusta_para, bgra_to_nv12};

fn mede(nome: &str, w: usize, h: usize, fps: u32, mut f: impl FnMut()) {
    // Aquece: a primeira passada paga falta de página e cache frio.
    for _ in 0..5 {
        f();
    }
    // Mínimo de várias rodadas, e não média: a máquina tem outros processos
    // e a média mede o ruído deles. O mínimo é o custo real do código.
    let mut por_frame = f64::MAX;
    for _ in 0..8 {
        let n = 20;
        let t0 = Instant::now();
        for _ in 0..n {
            f();
        }
        por_frame = por_frame.min(t0.elapsed().as_secs_f64() / f64::from(n));
    }
    println!(
        "{nome:30} {w}x{h}@{fps:<3} {:>7.2} ms/frame   {:>5.1}% de um núcleo",
        por_frame * 1000.0,
        por_frame * f64::from(fps) * 100.0
    );
}

fn main() {
    for (w, h) in [(1920usize, 1080usize), (2560, 1440)] {
        let bgra = vec![128u8; w * h * 4];
        let nv12 = vec![128u8; w * h * 3 / 2];
        let mut dst = Vec::new();

        for fps in [30u32, 60] {
            mede("bgra_to_nv12", w, h, fps, || {
                std::hint::black_box(bgra_to_nv12(std::hint::black_box(&bgra), w, h));
            });
        }
        mede("ajusta_para (mesmo tamanho)", w, h, 60, || {
            ajusta_para(&nv12, w, h, w, h, &mut dst);
        });
        mede("ajusta_para (reescalando)", w, h, 60, || {
            ajusta_para(&nv12, w, h, 1280, 720, &mut dst);
        });
        println!();
    }
}
