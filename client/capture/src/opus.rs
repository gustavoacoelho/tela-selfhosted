//! Encode de áudio em Opus, em processo.
//!
//! # Por que existe
//!
//! O áudio era o último motivo de o `ffmpeg.exe` continuar existindo. Com o
//! vídeo encodado em processo, aquele processo ficava vivo só para transformar
//! ~128 kbps de PCM em Opus — e arrastava junto um named pipe, supervisão de
//! processo, um salto RTP por UDP e o remendo de numeração que existia para
//! sobreviver ao reinício dele.
//!
//! Não é o que os aplicativos do ramo fazem: OBS, Discord e o WebRTC do Chrome
//! encodam áudio no próprio processo. A fronteira de processo só acrescenta
//! latência e um problema de sincronia que não precisava existir.
//!
//! A libopus já vem dentro do `avcodec` que baixamos, então isto não custa um
//! byte a mais de distribuição.
//!
//! # Marcação de tempo
//!
//! O tempo do áudio vem da CONTAGEM DE AMOSTRAS, não do relógio. É monotônico
//! por construção, e o `pump` já entrega no ritmo do relógio preenchendo
//! silêncio quando o aplicativo está mudo. Carimbar pelo relógio faria o
//! oposto do que parece: leitura de dispositivo acontece em rajadas, e os
//! carimbos andariam para trás.

use std::ffi::{c_int, CString};
use std::io::Write;
use std::ptr;

use anyhow::{bail, Result};

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::libav::{self, tipos as ff};

/// `AV_SAMPLE_FMT_S16`: inteiro de 16 bits, canais intercalados.
///
/// É exatamente o que o WASAPI entrega depois da conversão do `audio.rs`, e o
/// libopus aceita. Sem reamostragem no caminho.
const FORMATO: ff::AVSampleFormat = 1;

fn av() -> &'static libav::Api {
    libav::api().expect("a libav não foi carregada — ver libav::carrega")
}

/// Encoder de Opus.
pub struct Opus {
    codec: *mut ff::AVCodecContext,
    quadro: *mut ff::AVFrame,
    pacote: *mut ff::AVPacket,
    /// Amostras por canal que o encoder exige por quadro.
    ///
    /// O Opus não aceita qualquer tamanho: são blocos de 2,5 a 60 ms. O
    /// libopus informa o dele depois de aberto, e é a ele que obedecemos.
    amostras: usize,
    /// Bytes de PCM que chegaram e ainda não completam um quadro.
    sobra: Vec<u8>,
    /// Contagem de apresentação, em amostras.
    pts: i64,
}

// Os ponteiros são de objetos que a libav não prende a thread nenhuma; o acesso
// serializado é garantido por haver um único dono (a thread do áudio).
unsafe impl Send for Opus {}

impl Opus {
    pub fn novo(kbps: u32) -> Result<Self> {
        unsafe {
            let nome = CString::new("libopus")?;
            let codec = (av().avcodec_find_encoder_by_name)(nome.as_ptr());
            if codec.is_null() {
                bail!("esta build da libavcodec não tem libopus");
            }
            let mut cc = (av().avcodec_alloc_context3)(codec);
            (*cc).sample_rate = SAMPLE_RATE as c_int;
            (*cc).sample_fmt = FORMATO;
            (*cc).bit_rate = i64::from(kbps) * 1000;
            (*cc).time_base = ff::AVRational {
                num: 1,
                den: SAMPLE_RATE as c_int,
            };
            (av().av_channel_layout_default)(&mut (*cc).ch_layout, CHANNELS as c_int);

            let rc = (av().avcodec_open2)(cc, codec, ptr::null_mut());
            if rc < 0 {
                (av().avcodec_free_context)(&mut cc);
                bail!("abrir o libopus: {}", crate::encoder::texto_do_erro(rc));
            }

            // Depois de aberto o encoder informa o tamanho de bloco dele.
            // Zero seria "aceita qualquer tamanho", o que o Opus nunca é;
            // 960 (20 ms a 48 kHz) é o que ele usa.
            let amostras = if (*cc).frame_size > 0 {
                (*cc).frame_size as usize
            } else {
                960
            };

            let quadro = (av().av_frame_alloc)();
            (*quadro).format = FORMATO;
            (*quadro).nb_samples = amostras as c_int;
            (av().av_channel_layout_default)(&mut (*quadro).ch_layout, CHANNELS as c_int);
            let rc = (av().av_frame_get_buffer)(quadro, 0);
            if rc < 0 {
                let mut q = quadro;
                (av().av_frame_free)(&mut q);
                (av().avcodec_free_context)(&mut cc);
                bail!(
                    "memória para o quadro de áudio: {}",
                    crate::encoder::texto_do_erro(rc)
                );
            }

            Ok(Self {
                codec: cc,
                quadro,
                pacote: (av().av_packet_alloc)(),
                amostras,
                sobra: Vec::new(),
                pts: 0,
            })
        }
    }

    /// Quantos bytes de PCM formam um quadro.
    #[must_use]
    pub const fn bytes_por_quadro(&self) -> usize {
        self.amostras * CHANNELS * 2
    }

    /// Encoda o PCM que couber em quadros inteiros e guarda a sobra.
    ///
    /// `ao_sair` recebe cada pacote Opus e o carimbo dele em amostras.
    pub fn encoda(
        &mut self,
        pcm: &[u8],
        ao_sair: &mut dyn FnMut(&[u8], i64) -> Result<()>,
    ) -> Result<()> {
        let cheio = self.bytes_por_quadro();
        self.sobra.extend_from_slice(pcm);
        while self.sobra.len() >= cheio {
            // Copia para o quadro e remove do começo. `drain` move o resto, que
            // é no máximo um quadro — irrelevante a 128 kbps.
            unsafe {
                let destino = (*self.quadro).data[0];
                ptr::copy_nonoverlapping(self.sobra.as_ptr(), destino, cheio);
                (*self.quadro).pts = self.pts;
            }
            self.sobra.drain(..cheio);
            self.pts += self.amostras as i64;

            unsafe {
                let rc = (av().avcodec_send_frame)(self.codec, self.quadro);
                if rc < 0 {
                    bail!(
                        "avcodec_send_frame (áudio): {}",
                        crate::encoder::texto_do_erro(rc)
                    );
                }
                loop {
                    let rc = (av().avcodec_receive_packet)(self.codec, self.pacote);
                    if rc == libav::AVERRO_DE_NOVO || rc == libav::AVERRO_FIM {
                        break;
                    }
                    if rc < 0 {
                        bail!(
                            "avcodec_receive_packet (áudio): {}",
                            crate::encoder::texto_do_erro(rc)
                        );
                    }
                    let p = &*self.pacote;
                    let bytes = std::slice::from_raw_parts(p.data, p.size as usize);
                    let resultado = ao_sair(bytes, p.pts);
                    (av().av_packet_unref)(self.pacote);
                    resultado?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for Opus {
    fn drop(&mut self) {
        unsafe {
            if !self.pacote.is_null() {
                (av().av_packet_free)(&mut self.pacote);
            }
            if !self.quadro.is_null() {
                (av().av_frame_free)(&mut self.quadro);
            }
            if !self.codec.is_null() {
                (av().avcodec_free_context)(&mut self.codec);
            }
        }
    }
}

/// Adapta o [`Opus`] ao `Write` que o `audio::pump` alimenta.
///
/// O pump escreve num sumidouro genérico — era um named pipe, agora é isto.
/// Nada em `audio.rs` precisou mudar.
pub struct Sumidouro<F> {
    opus: Opus,
    ao_sair: F,
}

impl<F: FnMut(&[u8], i64) -> Result<()>> Sumidouro<F> {
    pub fn novo(kbps: u32, ao_sair: F) -> Result<Self> {
        Ok(Self {
            opus: Opus::novo(kbps)?,
            ao_sair,
        })
    }
}

impl<F: FnMut(&[u8], i64) -> Result<()>> Write for Sumidouro<F> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Self { opus, ao_sair } = self;
        opus.encoda(buf, ao_sair)
            .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Nada a esvaziar: o que sobrou não completa um quadro de Opus, e
        // meio quadro não existe. São no máximo 20 ms no fim da transmissão.
        Ok(())
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    fn pronto() -> bool {
        if crate::libav::carrega_de_teste() {
            return true;
        }
        eprintln!("sem vendor/ffmpeg, pulando");
        false
    }

    /// Uma senoide, para haver som de verdade a comprimir.
    fn tom(amostras: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(amostras * CHANNELS * 2);
        for i in 0..amostras {
            #[allow(clippy::cast_possible_truncation)]
            let s = ((i as f64 * 440.0 * 2.0 * std::f64::consts::PI / f64::from(SAMPLE_RATE)).sin()
                * 8000.0) as i16;
            for _ in 0..CHANNELS {
                v.extend_from_slice(&s.to_le_bytes());
            }
        }
        v
    }

    #[test]
    fn encoda_e_o_tempo_anda_com_as_amostras() {
        if !pronto() {
            return;
        }
        let mut opus = Opus::novo(128).expect("abrir libopus");
        let por_quadro = opus.bytes_por_quadro();
        assert!(por_quadro > 0);

        let mut pacotes = Vec::new();
        let mut coleta = |b: &[u8], pts: i64| {
            pacotes.push((b.len(), pts));
            Ok(())
        };
        // Um segundo de som.
        opus.encoda(&tom(SAMPLE_RATE as usize), &mut coleta)
            .expect("encoda");

        assert!(!pacotes.is_empty(), "não saiu pacote de Opus");
        assert!(
            pacotes.iter().all(|(n, _)| *n > 0),
            "algum pacote saiu vazio"
        );
        // O carimbo anda em blocos iguais, que é o que o receptor espera.
        let passo = por_quadro / (CHANNELS * 2);
        for par in pacotes.windows(2) {
            assert_eq!(
                par[1].1 - par[0].1,
                passo as i64,
                "o tempo do áudio não andou em blocos de {passo} amostras"
            );
        }
    }

    #[test]
    fn guarda_a_sobra_entre_chamadas() {
        if !pronto() {
            return;
        }
        let mut opus = Opus::novo(128).expect("abrir libopus");
        let por_quadro = opus.bytes_por_quadro();

        // `Cell` para o contador poder ser lido entre as duas chamadas sem
        // brigar com o empréstimo da closure.
        let quantos = std::cell::Cell::new(0usize);
        let mut conta = |_: &[u8], _: i64| {
            quantos.set(quantos.get() + 1);
            Ok(())
        };
        // Meio quadro de cada vez: nada sai na primeira, e as duas juntas
        // formam exatamente um. É o caso real — o pump entrega em lotes que
        // não têm relação com o bloco do Opus.
        let metade = por_quadro / 2 / (CHANNELS * 2);
        opus.encoda(&tom(metade), &mut conta).expect("primeira");
        assert_eq!(quantos.get(), 0, "saiu pacote com meio quadro");
        opus.encoda(&tom(metade), &mut conta).expect("segunda");
        assert_eq!(
            quantos.get(),
            1,
            "as duas metades deveriam formar um quadro"
        );
    }

    #[test]
    fn o_sumidouro_aceita_qualquer_tamanho_de_escrita() {
        if !pronto() {
            return;
        }
        let mut saiu = 0usize;
        let mut sink = Sumidouro::novo(128, |_: &[u8], _: i64| {
            saiu += 1;
            Ok(())
        })
        .expect("sumidouro");
        // Lotes irregulares, como o pump entrega.
        for n in [100usize, 4000, 37, 9000, 1] {
            sink.write_all(&tom(n)).expect("write");
        }
        sink.flush().expect("flush");
        assert!(saiu > 0, "o sumidouro não produziu pacote nenhum");
    }
}
