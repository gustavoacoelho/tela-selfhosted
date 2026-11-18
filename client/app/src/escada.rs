//! Escada de qualidade: escolhe resolução e fps, além da taxa.
//!
//! # Por que a taxa sozinha não basta
//!
//! Abaixo de certo ponto, 1080p60 fica PIOR que 720p30 na mesma taxa: não
//! sobram bits por pixel e a imagem vira bloco. Um controlador que só mexe no
//! bitrate tem fundo do poço — chega numa taxa onde nada do que ele faz
//! melhora. Reduzir pixel é o que salva o caso ruim.
//!
//! # De onde vieram os números
//!
//! A estrutura é a do libwebrtc, que é o motor do Meet e do Discord: cada
//! degrau tem um `min` (abaixo dele, desce) e um `max` (cravado nele com rede
//! limpa, sobe). As faixas se sobrepõem de propósito — a preferência é MANTER
//! a resolução, não trocar no ponto de cruzamento.
//!
//! Os valores do libwebrtc são para câmera a 30 fps; os daqui são mais altos
//! porque jogo a 60 é mais difícil de comprimir. O teto de 1080p60 em 8 Mbps
//! segue o que Discord e Twitch praticam.
//!
//! Uma medição própria com VMAF ficou inconclusiva e por isso não entrou:
//! o limiar entre resoluções depende MUITO do conteúdo — numa fonte de cores
//! chapadas 1080p já ganhava a 1500 kbps, numa de detalhe fino só passava a
//! ganhar depois de 3500. Um limiar fixo erra para um dos dois lados, e não
//! existe sinal barato em tempo de execução que resolva: o quantizador do
//! encoder, que era o candidato, dá o mesmo valor para conteúdos com
//! qualidade radicalmente diferente. Ficou o conservador.

use crate::bitrate::{self, Decisao};

/// Um degrau: resolução, fps e a faixa de taxa em que ele faz sentido.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Degrau {
    /// Altura alvo. `0` = resolução nativa da fonte, sem reescalar.
    pub altura: u32,
    pub fps: u32,
    /// Abaixo disto a resolução não se sustenta: desce um degrau.
    pub min: u32,
    /// Cravado aqui com rede limpa há bastante tempo: sobe um degrau.
    pub max: u32,
}

/// Do pior para o melhor. A ordem é o que dá sentido a "subir" e "descer".
pub const ESCADA: [Degrau; 4] = [
    Degrau {
        altura: 720,
        fps: 30,
        min: 800,
        max: 4000,
    },
    Degrau {
        altura: 720,
        fps: 60,
        min: 1500,
        max: 4500,
    },
    Degrau {
        altura: 1080,
        fps: 60,
        min: 3000,
        max: 8000,
    },
    Degrau {
        altura: 0,
        fps: 60,
        min: 6000,
        max: 12000,
    },
];

/// Onde o automático começa: 1080p60.
///
/// Não começa na nativa de propósito. Numa fonte de 1440p isso seria estrear
/// mandando 1,78x mais pixels e descer degrau por degrau até achar o lugar,
/// com um reinício em cada. E "automático" que dobra o consumo de banda
/// sozinho surpreende no sentido ruim.
pub const INICIO: usize = 2;

/// Amostras limpas cravado no teto antes de subir de degrau.
///
/// Muito mais que o do bitrate (60 = 2 min) porque mudar resolução é
/// perceptível: a imagem amolece na descida e endurece na subida. Descer é
/// urgente e usa a mesma evidência do bitrate; subir é oportunista.
const LIMPAS_PARA_SUBIR: u32 = 150; // 5 minutos a 2 s por amostra

/// Tolerância para considerar a taxa "cravada" no teto do degrau.
const PERTO_DO_TETO: f64 = 0.95;

/// O que fazer depois de uma amostra.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ajuste {
    Nada,
    /// Índice do degrau e taxa a aplicar.
    Muda {
        degrau: usize,
        kbps: u32,
        motivo: String,
    },
}

pub struct Auto {
    indice: usize,
    /// Degrau mais alto permitido. Nativa só entra quando a fonte é maior que
    /// 1080p — abaixo disso ela é idêntica ao degrau de 1080p e subir seria
    /// reiniciar o encoder para não mudar nada.
    teto: usize,
    taxa: bitrate::Controlador,
    limpas: u32,
}

impl Auto {
    /// `altura_da_fonte` decide se o degrau nativo existe.
    #[must_use]
    pub fn novo(altura_da_fonte: u32) -> Self {
        let teto = if altura_da_fonte > 1080 {
            ESCADA.len() - 1
        } else {
            ESCADA.len() - 2
        };
        let indice = INICIO.min(teto);
        Self {
            indice,
            teto,
            taxa: bitrate::Controlador::novo(ESCADA[indice].max),
            limpas: 0,
        }
    }

    #[must_use]
    pub fn degrau(&self) -> Degrau {
        ESCADA[self.indice]
    }

    #[must_use]
    pub fn kbps(&self) -> u32 {
        self.taxa.atual()
    }

    /// Registra uma medida e devolve o que fazer.
    pub fn amostra(&mut self, perda_por_mil: u64) -> Ajuste {
        let atual = ESCADA[self.indice];
        let decisao = self.taxa.amostra(perda_por_mil);

        if perda_por_mil == 0 && self.taxa.atual() >= (f64::from(atual.max) * PERTO_DO_TETO) as u32
        {
            self.limpas = self.limpas.saturating_add(1);
        } else {
            self.limpas = 0;
        }

        // Descer: o bitrate encostou no piso do degrau e a perda continua.
        // Mais pixels não cabem, então tira pixels.
        if let Decisao::Muda { kbps, motivo } = decisao {
            if kbps <= atual.min && self.indice > 0 {
                // Motivo PRÓPRIO, não o da decisão de taxa.
                //
                // A condição dispara em qualquer direção: uma SUBIDA que ainda
                // fica abaixo do mínimo do degrau também significa que o
                // degrau está errado. Reaproveitar o motivo da taxa produzia
                // texto contraditório — visto ao vivo, "rede limpa há dois
                // minutos; menos pixels cabem melhor" numa descida de
                // resolução. Certo na ação, absurdo na explicação.
                return self.desce(&format!("{kbps} kbps não sustenta esta resolução"));
            }
            return Ajuste::Muda {
                degrau: self.indice,
                kbps,
                motivo: motivo.texto().to_string(),
            };
        }

        // Subir: cravado no teto, sem perda, por muito tempo.
        if self.indice < self.teto && self.limpas >= LIMPAS_PARA_SUBIR {
            return self.sobe();
        }

        Ajuste::Nada
    }

    fn desce(&mut self, motivo: &str) -> Ajuste {
        self.indice -= 1;
        self.aplica(format!("{motivo} — menos pixels cabem melhor"))
    }

    fn sobe(&mut self) -> Ajuste {
        self.indice += 1;
        self.aplica("sobra banda há cinco minutos".to_string())
    }

    /// Reinicia o controlador de taxa no teto do degrau novo.
    ///
    /// No teto e não no meio: o degrau novo tem outra faixa, e começar por
    /// baixo faria a pessoa esperar minutos de subida gradual para chegar onde
    /// o degrau já dizia que dá.
    fn aplica(&mut self, motivo: String) -> Ajuste {
        let d = ESCADA[self.indice];
        self.taxa = bitrate::Controlador::novo(d.max);
        self.limpas = 0;
        Ajuste::Muda {
            degrau: self.indice,
            kbps: d.max,
            motivo,
        }
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    fn roda(a: &mut Auto, n: u32, perda: u64) -> Vec<Ajuste> {
        (0..n)
            .map(|_| a.amostra(perda))
            .filter(|x| *x != Ajuste::Nada)
            .collect()
    }

    /// Fonte de 1080p não tem degrau nativo: subir para ele seria reiniciar o
    /// encoder para mandar exatamente os mesmos pixels.
    #[test]
    fn fonte_de_1080p_nao_tem_degrau_nativo() {
        let a = Auto::novo(1080);
        assert_eq!(a.degrau().altura, 1080);
        assert_eq!(a.teto, 2, "para no 1080p60");
    }

    /// Fonte maior desbloqueia a nativa — mas o automático NÃO começa nela.
    #[test]
    fn fonte_maior_desbloqueia_a_nativa_mas_nao_comeca_nela() {
        let a = Auto::novo(1440);
        assert_eq!(a.teto, 3);
        assert_eq!(a.degrau().altura, 1080, "começa no meio, não no topo");
    }

    /// Rede limpa não mexe em nada por muito tempo. Uma escada que se agita
    /// sozinha é pior que uma escada que não existe.
    #[test]
    fn rede_limpa_fica_quieta() {
        let mut a = Auto::novo(1080);
        assert!(roda(&mut a, LIMPAS_PARA_SUBIR - 1, 0).is_empty());
        assert_eq!(a.degrau().altura, 1080);
    }

    /// Perda repetida derruba a taxa primeiro; só quando ela encosta no piso
    /// do degrau é que a resolução cai.
    #[test]
    fn a_taxa_cai_antes_da_resolucao() {
        let mut a = Auto::novo(1080);
        let mudancas = roda(&mut a, 400, 5);
        assert!(!mudancas.is_empty());

        // A primeira mudança tem de ser de taxa, no mesmo degrau.
        match &mudancas[0] {
            Ajuste::Muda { degrau, kbps, .. } => {
                assert_eq!(*degrau, 2, "não muda de degrau de cara");
                assert_eq!(*kbps, 6000, "8000 x 0,75");
            }
            Ajuste::Nada => unreachable!(),
        }
        // E em algum momento a resolução cede.
        assert!(
            a.degrau().altura < 1080,
            "com perda contínua a resolução tem de ceder"
        );
    }

    /// Nunca abaixo do degrau mais baixo, por pior que fique.
    #[test]
    fn nao_desce_do_primeiro_degrau() {
        let mut a = Auto::novo(1080);
        roda(&mut a, 3000, 40);
        assert_eq!(a.indice, 0);
        assert_eq!(a.degrau().altura, 720);
        assert_eq!(a.degrau().fps, 30);
    }

    /// Rede que sarou devolve resolução — devagar, e até o teto permitido.
    #[test]
    fn rede_limpa_por_muito_tempo_devolve_resolucao() {
        let mut a = Auto::novo(1440);
        roda(&mut a, 3000, 40);
        assert_eq!(a.indice, 0, "afundou");

        roda(&mut a, 20_000, 0);
        assert_eq!(a.indice, 3, "voltou até a nativa");
        assert_eq!(a.degrau().altura, 0);
    }

    /// Subir de degrau recomeça a taxa no teto do degrau novo, não do zero:
    /// o degrau já diz que aquela faixa cabe.
    #[test]
    fn subir_recomeca_a_taxa_no_teto_do_degrau() {
        let mut a = Auto::novo(1080);
        roda(&mut a, 3000, 40);
        assert_eq!(a.indice, 0);

        // Bastante amostra limpa: a subida de degrau só acontece depois de a
        // TAXA reencostar no teto do degrau atual, e ela sobe devagar, de
        // 20% em 20%, com carência entre cada. Exigir isso é de propósito —
        // subir resolução com o bitrate no meio da faixa daria uma imagem
        // pior que a de antes.
        let subidas = roda(&mut a, 3000, 0);
        match subidas
            .iter()
            .find(|x| matches!(x, Ajuste::Muda { degrau, .. } if *degrau == 1))
        {
            Some(Ajuste::Muda { kbps, .. }) => assert_eq!(*kbps, ESCADA[1].max),
            _ => panic!("devia ter subido para o degrau 1"),
        }
    }

    /// A escada tem de ser monotônica nas duas pontas, senão "subir" e
    /// "descer" não significam nada.
    #[test]
    fn a_escada_e_crescente() {
        for par in ESCADA.windows(2) {
            let (a, b) = (par[0], par[1]);
            assert!(b.min >= a.min, "min não cresce: {a:?} -> {b:?}");
            assert!(b.max >= a.max, "max não cresce: {a:?} -> {b:?}");
            // `u64` porque a nativa é representada por `altura: 0` e vira o
            // maior valor possível na comparação — em 32 bits isso estoura.
            let pixels = |d: Degrau| {
                u64::from(if d.altura == 0 { u32::MAX } else { d.altura }) * u64::from(d.fps)
            };
            assert!(pixels(b) > pixels(a), "não melhora: {a:?} -> {b:?}");
        }
    }

    /// As faixas se sobrepõem, como no libwebrtc: a preferência é MANTER a
    /// resolução, não trocar no ponto de cruzamento. Sem sobreposição a escada
    /// oscilaria na fronteira.
    #[test]
    fn as_faixas_se_sobrepoem() {
        for par in ESCADA.windows(2) {
            let (a, b) = (par[0], par[1]);
            assert!(
                b.min < a.max,
                "faixas sem sobreposição entre {a:?} e {b:?}: a escada vai oscilar"
            );
        }
    }
}

/// Como a qualidade é decidida ao longo da transmissão.
///
/// As duas formas compartilham o controlador de taxa: mesmo com resolução
/// travada, o bitrate flutua. Travar a taxa também não serviria a ninguém —
/// quem escolhe "1080p60" quer aquela resolução, não quer perder pacote.
pub enum Politica {
    /// Preset manual: resolução e fps travados, taxa livre na faixa do degrau.
    Fixa {
        degrau: Degrau,
        taxa: bitrate::Controlador,
    },
    /// Automático: a escada inteira.
    Auto(Auto),
}

impl Politica {
    #[must_use]
    pub fn fixa(degrau: Degrau) -> Self {
        Self::Fixa {
            degrau,
            taxa: bitrate::Controlador::novo(degrau.max),
        }
    }

    #[must_use]
    pub fn auto(altura_da_fonte: u32) -> Self {
        Self::Auto(Auto::novo(altura_da_fonte))
    }

    /// Degrau em vigor agora.
    #[must_use]
    pub fn degrau(&self) -> Degrau {
        match self {
            Self::Fixa { degrau, .. } => *degrau,
            Self::Auto(a) => a.degrau(),
        }
    }

    /// Taxa em vigor agora.
    #[must_use]
    pub fn kbps(&self) -> u32 {
        match self {
            Self::Fixa { taxa, .. } => taxa.atual(),
            Self::Auto(a) => a.kbps(),
        }
    }

    /// Aceita uma taxa decidida de fora (agenda de teste).
    pub fn assume(&mut self, kbps: u32) {
        match self {
            Self::Fixa { taxa, .. } => taxa.assume(kbps),
            Self::Auto(a) => a.assume(kbps),
        }
    }

    /// Registra uma medida. `None` = nada a mudar.
    pub fn amostra(&mut self, perda_por_mil: u64) -> Option<(Degrau, u32, String)> {
        match self {
            Self::Fixa { degrau, taxa } => match taxa.amostra(perda_por_mil) {
                Decisao::Muda { kbps, motivo } => Some((*degrau, kbps, motivo.texto().to_string())),
                Decisao::Manter => None,
            },
            Self::Auto(a) => match a.amostra(perda_por_mil) {
                Ajuste::Muda {
                    degrau,
                    kbps,
                    motivo,
                } => Some((ESCADA[degrau], kbps, motivo)),
                Ajuste::Nada => None,
            },
        }
    }
}

impl Auto {
    /// Ver [`Politica::assume`].
    pub fn assume(&mut self, kbps: u32) {
        self.taxa.assume(kbps);
        self.limpas = 0;
    }
}

#[cfg(test)]
mod testes_politica {
    use super::*;

    /// Preset manual não muda de resolução, por pior que fique. Foi a escolha
    /// de quem transmite, e desrespeitá-la seria pior que a imagem ruim.
    #[test]
    fn preset_manual_nunca_troca_de_resolucao() {
        let mut p = Politica::fixa(ESCADA[2]);
        for _ in 0..3000 {
            if let Some((d, _, _)) = p.amostra(40) {
                assert_eq!(d, ESCADA[2], "trocou de degrau num preset fixo");
            }
        }
        assert_eq!(p.degrau().altura, 1080);
    }

    /// Mas a TAXA flutua: quem escolheu 1080p60 quer aquela resolução, não
    /// quer perder pacote.
    #[test]
    fn preset_manual_ainda_ajusta_a_taxa() {
        let mut p = Politica::fixa(ESCADA[2]);
        let inicial = p.kbps();
        let mut mudou = false;
        for _ in 0..200 {
            if p.amostra(5).is_some() {
                mudou = true;
            }
        }
        assert!(mudou, "a taxa tinha de ter cedido");
        assert!(p.kbps() < inicial, "{} -> {}", inicial, p.kbps());
    }

    /// O automático troca de resolução quando precisa.
    #[test]
    fn automatico_troca_de_resolucao() {
        let mut p = Politica::auto(1080);
        for _ in 0..3000 {
            p.amostra(40);
        }
        assert!(p.degrau().altura < 1080);
    }
}

impl Politica {
    /// Monta a política a partir do preset escolhido na interface.
    ///
    /// `altura_da_fonte` só importa para o automático, que usa ela para saber
    /// se o degrau nativo existe. Desconhecido cai no automático: é o padrão,
    /// e é o que menos exige da pessoa saber.
    #[must_use]
    pub fn do_preset(nome: &str, altura_da_fonte: u32) -> Self {
        match nome {
            "leve" => Self::fixa(ESCADA[0]),
            "qualidade" => Self::fixa(ESCADA[2]),
            // Numa fonte de 1080p ou menos, "nativa" É o degrau de 1080p:
            // `escala_para` não amplia, então os dois mandam os mesmos pixels.
            // O que muda é só o teto de taxa, e o de 1080p é o honesto.
            "nativa" if altura_da_fonte > 1080 => Self::fixa(ESCADA[3]),
            "nativa" => Self::fixa(ESCADA[2]),
            _ => Self::auto(altura_da_fonte),
        }
    }
}

#[cfg(test)]
mod testes_preset {
    use super::*;

    #[test]
    fn cada_preset_vira_o_degrau_certo() {
        assert_eq!(Politica::do_preset("leve", 1080).degrau(), ESCADA[0]);
        assert_eq!(Politica::do_preset("qualidade", 1080).degrau(), ESCADA[2]);
        assert_eq!(Politica::do_preset("nativa", 1440).degrau(), ESCADA[3]);
    }

    /// "Nativa" numa fonte de 1080p não pode virar um degrau com teto de 12
    /// Mbps: `escala_para` não amplia, então seriam os mesmos pixels do 1080p
    /// com permissão para gastar 50% mais banda à toa.
    #[test]
    fn nativa_em_fonte_pequena_vira_1080p() {
        assert_eq!(Politica::do_preset("nativa", 1080).degrau(), ESCADA[2]);
        assert_eq!(Politica::do_preset("nativa", 720).degrau(), ESCADA[2]);
    }

    /// O padrão é o automático, inclusive para nome desconhecido — versão
    /// nova da interface contra binário velho, ou config editada à mão.
    #[test]
    fn desconhecido_cai_no_automatico() {
        for nome in ["auto", "", "abacaxi", "trabalho", "esporte", "jogo"] {
            assert!(
                matches!(Politica::do_preset(nome, 1080), Politica::Auto(_)),
                "{nome:?} devia cair no automático"
            );
        }
    }
}

#[cfg(test)]
mod testes_motivo {
    use super::*;

    /// O motivo da descida de resolução tem de ser DELA, não o da decisão de
    /// taxa que a disparou.
    ///
    /// A condição de descer dispara em qualquer direção: uma subida de taxa
    /// que ainda fica abaixo do mínimo do degrau também prova que o degrau
    /// está errado. Reaproveitar o motivo produzia texto contraditório, visto
    /// ao vivo: "rede limpa há dois minutos; menos pixels cabem melhor" numa
    /// DESCIDA de resolução.
    #[test]
    fn descida_por_taxa_baixa_nao_fala_em_rede_limpa() {
        let mut a = Auto::novo(1440);
        // Força a taxa bem abaixo do mínimo do degrau nativo (6000).
        a.assume(2000);
        // Rede limpa por tempo suficiente para a taxa tentar SUBIR.
        let mut visto = None;
        for _ in 0..3000 {
            if let Ajuste::Muda { degrau, motivo, .. } = a.amostra(0) {
                if degrau < 3 {
                    visto = Some(motivo);
                    break;
                }
            }
        }
        let motivo = visto.expect("devia ter descido de degrau");
        assert!(
            !motivo.contains("rede limpa"),
            "motivo contraditório numa descida: {motivo:?}"
        );
        assert!(
            motivo.contains("não sustenta"),
            "o motivo devia explicar a taxa insuficiente: {motivo:?}"
        );
    }
}
