//! Redução de resolução na GPU, com o processador de vídeo do D3D11.
//!
//! # Por que existe
//!
//! Os presets e a escada reduzem resolução — 1080p60 para 720p30 quando a rede
//! aperta, e 1440p para 1080p sempre, porque mandar 1440p custaria banda de
//! quem assiste sem ninguém pedir. No caminho antigo quem reescalava era o
//! `-vf scale` do ffmpeg, na CPU. Encodando a textura direto, isso precisa
//! acontecer na GPU ou o caminho novo perde o que veio ganhar.
//!
//! # Por que o VideoProcessor e não um shader
//!
//! É a mesma peça de hardware que o decodificador de vídeo usa para escalar, e
//! vem pronta: sem shader para compilar, sem malha, sem estado de pipeline
//! para salvar e restaurar. Um quad com pixel shader daria mais controle sobre
//! o filtro, mas controle não é o que falta aqui — o gargalo perceptual é o
//! bitrate, não a qualidade da reamostragem.
//!
//! # O que NÃO faz
//!
//! Não aumenta. Pedir 1080p de uma fonte 720p só gastaria bits para inventar
//! pixel que não existe; quem chama trata isso mandando no tamanho nativo.

use anyhow::{Context as _, Result};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_TEX2D_ARRAY_VPOV, D3D11_TEX2D_VPIV,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL;

/// Um redutor de resolução preso a um par (origem, destino).
///
/// Fica preso ao par porque o `ID3D11VideoProcessor` é criado a partir das
/// duas resoluções; mudar qualquer uma exige um novo. Como a mudança acontece
/// no máximo a cada poucos minutos (é a escada decidindo), recriar sai mais
/// barato que carregar a generalidade.
pub struct Escalador {
    contexto: ID3D11VideoContext,
    processador: ID3D11VideoProcessor,
    enumerador: ID3D11VideoProcessorEnumerator,
    dispositivo: ID3D11VideoDevice,
    origem: (u32, u32),
    destino: (u32, u32),
}

impl Escalador {
    pub fn novo(
        dispositivo: &ID3D11Device,
        contexto: &ID3D11DeviceContext,
        origem: (u32, u32),
        destino: (u32, u32),
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice = dispositivo
            .cast()
            .context("esta GPU não expõe ID3D11VideoDevice")?;
        let video_contexto: ID3D11VideoContext = contexto
            .cast()
            .context("esta GPU não expõe ID3D11VideoContext")?;

        // A taxa declarada aqui não muda o resultado de um `Blt` avulso: ela
        // serve para o driver escolher entre modos de desentrelaçamento, e o
        // nosso conteúdo é progressivo. Vai 60/1 por ser o teto do que
        // capturamos.
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: origem.0,
            InputHeight: origem.1,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: destino.0,
            OutputHeight: destino.1,
            // Velocidade, não qualidade: isto roda a cada quadro numa
            // máquina que também está rodando um jogo.
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };

        let enumerador = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }
            .context("CreateVideoProcessorEnumerator")?;
        let processador = unsafe { video_device.CreateVideoProcessor(&enumerador, 0) }
            .context("CreateVideoProcessor")?;

        Ok(Self {
            contexto: video_contexto,
            processador,
            enumerador,
            dispositivo: video_device,
            origem,
            destino,
        })
    }

    /// Para qual par este escalador serve.
    #[must_use]
    pub const fn par(&self) -> ((u32, u32), (u32, u32)) {
        (self.origem, self.destino)
    }

    /// Reduz `entrada` para dentro de `saida`.
    ///
    /// `sub_saida` é a camada dentro da textura em array do pool do encoder;
    /// `sub_entrada` é 0 para as texturas comuns que o WGC devolve.
    pub fn aplica(
        &self,
        entrada: &ID3D11Texture2D,
        sub_entrada: u32,
        saida: &ID3D11Texture2D,
        sub_saida: u32,
    ) -> Result<()> {
        // As views são por textura, e a do WGC muda a cada quadro — não há o
        // que cachear do lado da entrada. São objetos leves; o custo real está
        // no `Blt`, que roda no bloco de vídeo.
        let desc_entrada = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: sub_entrada,
                },
            },
        };
        let mut view_entrada: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            self.dispositivo.CreateVideoProcessorInputView(
                entrada,
                &self.enumerador,
                &desc_entrada,
                Some(&mut view_entrada),
            )
        }
        .context("CreateVideoProcessorInputView")?;

        // TEXTURE2DARRAY e não TEXTURE2D: o pool do encoder é UMA textura com
        // várias camadas (é o que `initial_pool_size` produz, e o que o AMF
        // espera). A camada de destino se diz aqui — o `OutputIndex` do fluxo
        // é outra coisa, serve para saída estéreo, e usá-lo faz o `Blt`
        // recusar com E_INVALIDARG.
        let desc_saida = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_VPOV {
                    MipSlice: 0,
                    FirstArraySlice: sub_saida,
                    ArraySize: 1,
                },
            },
        };
        let mut view_saida: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe {
            self.dispositivo.CreateVideoProcessorOutputView(
                saida,
                &self.enumerador,
                &desc_saida,
                Some(&mut view_saida),
            )
        }
        .context("CreateVideoProcessorOutputView")?;

        let (Some(view_entrada), Some(view_saida)) = (view_entrada, view_saida) else {
            anyhow::bail!("o D3D11 não devolveu as views do processador");
        };

        // Recortes explícitos. Sem eles o driver usa a textura inteira, o que
        // dá no mesmo hoje — mas o pool do encoder é uma textura em ARRAY, e
        // depender do padrão nesse caso é pedir para descobrir a diferença em
        // produção.
        let origem = RECT {
            left: 0,
            top: 0,
            right: self.origem.0 as i32,
            bottom: self.origem.1 as i32,
        };
        let destino = RECT {
            left: 0,
            top: 0,
            right: self.destino.0 as i32,
            bottom: self.destino.1 as i32,
        };
        unsafe {
            self.contexto.VideoProcessorSetStreamSourceRect(
                &self.processador,
                0,
                true,
                Some(&origem),
            );
            self.contexto.VideoProcessorSetStreamDestRect(
                &self.processador,
                0,
                true,
                Some(&destino),
            );
            self.contexto.VideoProcessorSetOutputTargetRect(
                &self.processador,
                true,
                Some(&destino),
            );
        }

        let fluxo = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            pInputSurface: core::mem::ManuallyDrop::new(Some(view_entrada.clone())),
            ..Default::default()
        };
        unsafe {
            self.contexto
                .VideoProcessorBlt(&self.processador, &view_saida, 0, &[fluxo])
        }
        .context("VideoProcessorBlt")
    }
}

/// A resolução de saída para uma fonte e uma altura desejada.
///
/// Devolve `None` quando não há o que reduzir — altura zero (pedido de
/// "nativo") ou fonte que já é menor que o alvo. Aumentar gastaria bits para
/// inventar pixel que não existe.
///
/// A largura sai proporcional e **par**, porque o H.264 exige dimensão par e
/// uma conta de proporção cai em ímpar na metade das vezes.
#[must_use]
pub fn reduz_para(largura: u32, altura: u32, alvo: u32) -> Option<(u32, u32)> {
    if alvo == 0 || altura == 0 || altura <= alvo {
        return None;
    }
    let nova_largura = (u64::from(largura) * u64::from(alvo) / u64::from(altura)) as u32;
    Some((nova_largura & !1, alvo & !1))
}

#[cfg(test)]
mod testes {
    use super::reduz_para;

    #[test]
    fn reduz_mantendo_a_proporcao() {
        assert_eq!(reduz_para(1920, 1080, 720), Some((1280, 720)));
        assert_eq!(reduz_para(2560, 1440, 1080), Some((1920, 1080)));
    }

    #[test]
    fn nao_aumenta() {
        // Fonte 720p pedindo 1080p: manda como está.
        assert_eq!(reduz_para(1280, 720, 1080), None);
        // Mesma altura também não tem o que fazer.
        assert_eq!(reduz_para(1280, 720, 720), None);
    }

    #[test]
    fn altura_zero_quer_dizer_nativo() {
        assert_eq!(reduz_para(3840, 2160, 0), None);
    }

    #[test]
    fn a_largura_sai_par() {
        // 1366x768 para 720 daria 1280,625 -> 1280. Um caso que arredonda.
        let (l, a) = reduz_para(1366, 768, 720).expect("reduz");
        assert_eq!(a % 2, 0, "altura ímpar: {a}");
        assert_eq!(l % 2, 0, "largura ímpar: {l}");
    }

    #[test]
    fn largura_impar_e_cortada() {
        // 1999x1000 para 500 daria 999,5 -> 999 -> 998.
        let (l, _) = reduz_para(1999, 1000, 500).expect("reduz");
        assert_eq!(l, 998);
    }
}

#[cfg(test)]
mod testes_gpu {
    use super::Escalador;
    use crate::encoder::{dispositivo_de_sonda, textura_de_sonda, Ajuste, Encoder};

    /// Sem a libav carregada nada aqui roda; ver `libav::carrega`.
    fn pronto() -> bool {
        if crate::libav::carrega_de_teste() {
            return true;
        }
        eprintln!("sem vendor/ffmpeg, pulando");
        false
    }

    /// Reduzir na GPU e encodar tem de sair quadro, não erro.
    ///
    /// Determinístico ao contrário do teste que captura a tela: não depende de
    /// haver movimento no monitor, que numa máquina parada não há.
    #[test]
    fn reduz_e_encoda() {
        if !pronto() {
            return;
        }
        let Ok((dev, ctx)) = dispositivo_de_sonda() else {
            eprintln!("sem D3D11, pulando");
            return;
        };
        let fonte = (1920, 1080);
        let saida = (1280, 720);
        let Ok(origem) = textura_de_sonda(&dev, fonte.0, fonte.1) else {
            eprintln!("sem textura, pulando");
            return;
        };
        let ajuste = Ajuste {
            largura: saida.0,
            altura: saida.1,
            fps: 60,
            kbps: 4000,
        };
        let Ok(mut enc) = Encoder::abre(&dev, &ctx, ajuste) else {
            eprintln!("sem encoder de hardware, pulando");
            return;
        };
        let escalador = Escalador::novo(&dev, &ctx, fonte, saida).expect("escalador");
        assert_eq!(escalador.par(), (fonte, saida));

        let mut quadros = 0usize;
        for _ in 0..30 {
            enc.preenche(|destino, camada| escalador.aplica(&origem, 0, destino, camada))
                .expect("preenche pela GPU");
            enc.drena(|_| quadros += 1).expect("drena");
        }
        assert!(quadros > 0, "reduziu na GPU e não saiu quadro nenhum");
    }
}
