//! Decide quando baixar e quando subir a taxa da transmissão.
//!
//! # Por que existe
//!
//! Medido na transmissão de alguém a 8000 kbps: perda de pacote em 16% das
//! amostras, mediana de 8 segundos entre eventos, tipicamente 3/1000 com
//! picos de 27/1000. A máquina dele estava impecável — zero quadro descartado
//! por nós, laço sem atraso, captura nos 60 quadros pedidos. Era o link de
//! subida não dando conta.
//!
//! Cada pacote RTP perdido é uma fatia de quadro que some, e o decodificador
//! mostra bloco corrompido até o próximo quadro-chave. A pessoa vê artefato de
//! poucos em poucos segundos e não tem como saber que a causa é a taxa que ela
//! mesma escolheu.
//!
//! # O que este módulo NÃO faz
//!
//! Não fala com o encoder. Ele só olha números e devolve uma decisão; quem
//! aplica é o supervisor, e aplicar custa reiniciar a transmissão. É por isso
//! que a histerese aqui é generosa: cada mudança tem preço para quem assiste,
//! então é melhor demorar a reagir do que oscilar.
//!
//! Separado justamente para poder ser testado sem encoder, sem rede e sem
//! esperar dois minutos por amostra.

use std::collections::VecDeque;

/// Amostras observadas antes de decidir baixar. A telemetria roda a cada 2s,
/// então são 10 segundos.
const JANELA: usize = 5;

/// Quantas amostras da janela precisam ter perda para a taxa cair.
///
/// Duas em cinco, e não uma: um pacote perdido isolado acontece em qualquer
/// rede e o decodificador se recupera no quadro-chave seguinte. Derrubar a
/// qualidade por causa dele seria trocar um artefato de meio segundo por uma
/// interrupção de dois. Com a mediana de 8 segundos entre perdas que medimos,
/// duas em dez segundos significam que o problema é contínuo.
const AMOSTRAS_RUINS: usize = 2;

/// Perda numa única amostra que dispensa esperar a janela encher.
///
/// 2% já destrói vários quadros seguidos. Esperar mais dez segundos para
/// confirmar o óbvio só prolonga o estrago.
const PERDA_GRAVE: u64 = 20;

/// Quanto a taxa cai de uma vez.
const FATOR_QUEDA: f64 = 0.75;

/// Quanto a taxa sobe de uma vez. Menor que a queda de propósito: descer é
/// urgente, subir é oportunista.
const FATOR_SUBIDA: f64 = 1.2;

/// Abaixo disto a imagem fica ruim de um jeito que não compensa mais.
const PISO_KBPS: u32 = 1500;

/// Amostras de silêncio antes de considerar qualquer coisa depois de mexer.
/// 30 x 2s = 1 minuto. Dá tempo do encoder novo estabilizar e da fila do
/// caminho esvaziar antes de julgar o resultado.
const CARENCIA: u32 = 30;

/// Amostras limpas seguidas para tentar subir. 60 x 2s = 2 minutos.
const LIMPAS_PARA_SUBIR: u32 = 60;

/// Teto de tentativas frustradas contadas para o recuo exponencial.
///
/// Sem ele, uma sessão longa numa rede ruim empurraria a espera para valores
/// absurdos por multiplicação sucessiva.
const MAX_RECUOS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decisao {
    Manter,
    /// Nova taxa em kbps, e por que mudou (para o log e para a interface).
    Muda {
        kbps: u32,
        motivo: Motivo,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motivo {
    /// Perda repetida na janela.
    PerdaSustentada,
    /// Uma amostra ruim o bastante para não esperar.
    PerdaGrave,
    /// Silêncio longo o suficiente para tentar de volta.
    RedeLimpa,
}

impl Motivo {
    #[must_use]
    pub fn texto(self) -> &'static str {
        match self {
            Self::PerdaSustentada => "perda de pacote repetida",
            Self::PerdaGrave => "perda de pacote alta",
            Self::RedeLimpa => "rede limpa há dois minutos",
        }
    }
}

pub struct Controlador {
    /// O que a pessoa escolheu. Nunca subimos acima disto: a decisão dela é o
    /// teto, não um palpite inicial.
    teto: u32,
    atual: u32,
    janela: VecDeque<u64>,
    /// Amostras restantes de carência depois de uma mudança.
    espera: u32,
    /// Amostras limpas seguidas acumuladas.
    limpas: u32,
    /// Quantas vezes uma subida foi seguida de queda. Cada uma dobra a espera
    /// exigida antes da próxima tentativa.
    recuos: u32,
    /// Se a última mudança foi uma subida, para saber se ela deu errado.
    ultima_foi_subida: bool,
}

impl Controlador {
    #[must_use]
    pub fn novo(teto: u32) -> Self {
        Self {
            teto,
            atual: teto,
            janela: VecDeque::with_capacity(JANELA),
            espera: CARENCIA,
            limpas: 0,
            recuos: 0,
            ultima_foi_subida: false,
        }
    }

    #[must_use]
    pub fn atual(&self) -> u32 {
        self.atual
    }

    /// Aceita uma taxa decidida de fora e recomeça a partir dela.
    ///
    /// Sem isto o controlador continuaria calculando a partir da taxa que ELE
    /// escolheu por último: uma troca externa para 6000 seguida de uma queda
    /// dele daria 8000 x 0,75, não 6000 x 0,75 — e a conta partiria de um
    /// número que não existe mais. A carência entra junto porque o encoder
    /// novo precisa do mesmo tempo para estabilizar, tenha a troca vindo de
    /// onde tiver vindo.
    pub fn assume(&mut self, kbps: u32) {
        self.atual = kbps.clamp(PISO_KBPS, self.teto.max(PISO_KBPS));
        self.espera = CARENCIA;
        self.limpas = 0;
        self.janela.clear();
        self.ultima_foi_subida = false;
    }

    /// Registra uma medida e devolve o que fazer.
    pub fn amostra(&mut self, perda_por_mil: u64) -> Decisao {
        self.janela.push_back(perda_por_mil);
        if self.janela.len() > JANELA {
            self.janela.pop_front();
        }

        if perda_por_mil == 0 {
            self.limpas = self.limpas.saturating_add(1);
        } else {
            self.limpas = 0;
        }

        // Carência: continua acumulando histórico, mas não decide nada.
        if self.espera > 0 {
            self.espera -= 1;
            return Decisao::Manter;
        }

        let ruins = self.janela.iter().filter(|p| **p > 0).count();
        let grave = perda_por_mil >= PERDA_GRAVE;

        if (grave || ruins >= AMOSTRAS_RUINS) && self.atual > PISO_KBPS {
            let novo = ((f64::from(self.atual) * FATOR_QUEDA).round() as u32).max(PISO_KBPS);
            // Uma subida seguida de queda significa que o teto real é mais
            // baixo do que tentamos. Cada uma dobra a espera da próxima.
            if self.ultima_foi_subida {
                self.recuos = (self.recuos + 1).min(MAX_RECUOS);
            }
            return self.aplica(
                novo,
                if grave {
                    Motivo::PerdaGrave
                } else {
                    Motivo::PerdaSustentada
                },
                false,
            );
        }

        if self.atual < self.teto && self.limpas >= self.limpas_exigidas() {
            let novo = ((f64::from(self.atual) * FATOR_SUBIDA).round() as u32).min(self.teto);
            if novo > self.atual {
                return self.aplica(novo, Motivo::RedeLimpa, true);
            }
        }

        Decisao::Manter
    }

    /// Quantas amostras limpas exigir antes de tentar subir de novo.
    fn limpas_exigidas(&self) -> u32 {
        LIMPAS_PARA_SUBIR.saturating_mul(1 << self.recuos)
    }

    fn aplica(&mut self, novo: u32, motivo: Motivo, subida: bool) -> Decisao {
        self.atual = novo;
        self.espera = CARENCIA;
        self.limpas = 0;
        self.janela.clear();
        self.ultima_foi_subida = subida;
        Decisao::Muda { kbps: novo, motivo }
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    /// Alimenta `n` amostras iguais e devolve as decisões que não foram
    /// "manter" — que é o que interessa em quase todo teste.
    fn roda(c: &mut Controlador, n: u32, perda: u64) -> Vec<Decisao> {
        (0..n)
            .map(|_| c.amostra(perda))
            .filter(|d| *d != Decisao::Manter)
            .collect()
    }

    /// Rede limpa não mexe em nada. É o caso da maioria das transmissões, e
    /// mexer nelas seria estragar o que funciona.
    #[test]
    fn rede_limpa_no_teto_nao_muda_nada() {
        let mut c = Controlador::novo(8000);
        assert!(roda(&mut c, 300, 0).is_empty());
        assert_eq!(c.atual(), 8000);
    }

    /// Perda isolada não derruba a qualidade: acontece em qualquer rede e o
    /// quadro-chave seguinte conserta. Derrubar custaria mais que o artefato.
    #[test]
    fn perda_isolada_nao_derruba() {
        let mut c = Controlador::novo(8000);
        roda(&mut c, CARENCIA, 0);
        for _ in 0..10 {
            assert_eq!(c.amostra(3), Decisao::Manter);
            // Uma ruim a cada seis, nunca duas dentro da mesma janela de cinco.
            assert!(roda(&mut c, 6, 0).is_empty());
        }
        assert_eq!(c.atual(), 8000);
    }

    /// O caso real que motivou o módulo: perda recorrente a 8000 kbps.
    #[test]
    fn perda_recorrente_baixa_a_taxa() {
        let mut c = Controlador::novo(8000);
        roda(&mut c, CARENCIA, 0);
        assert_eq!(c.amostra(3), Decisao::Manter, "uma só não basta");
        match c.amostra(3) {
            Decisao::Muda { kbps, motivo } => {
                assert_eq!(kbps, 6000, "8000 x 0,75");
                assert_eq!(motivo, Motivo::PerdaSustentada);
            }
            Decisao::Manter => panic!("duas na janela têm de baixar"),
        }
    }

    /// Perda alta não espera a janela encher.
    #[test]
    fn perda_grave_reage_na_primeira() {
        let mut c = Controlador::novo(8000);
        roda(&mut c, CARENCIA, 0);
        match c.amostra(27) {
            Decisao::Muda { motivo, .. } => assert_eq!(motivo, Motivo::PerdaGrave),
            Decisao::Manter => panic!("27/1000 numa amostra é para reagir na hora"),
        }
    }

    /// Depois de mexer, fica quieto por um minuto. Sem isto o controlador
    /// julgaria o encoder novo antes de ele estabilizar e desceria em degrau
    /// até o piso de uma vez só.
    #[test]
    fn nao_mexe_duas_vezes_seguidas() {
        let mut c = Controlador::novo(8000);
        roda(&mut c, CARENCIA, 0);
        roda(&mut c, 2, 5); // desce para 6000
        assert_eq!(c.atual(), 6000);
        assert!(
            roda(&mut c, CARENCIA, 30).is_empty(),
            "durante a carência não mexe nem com perda grave"
        );
        assert_eq!(c.atual(), 6000);
    }

    /// Nunca abaixo do piso, por pior que fique.
    #[test]
    fn respeita_o_piso() {
        let mut c = Controlador::novo(8000);
        for _ in 0..40 {
            roda(&mut c, CARENCIA + 2, 30);
        }
        assert_eq!(c.atual(), PISO_KBPS);
        assert!(
            roda(&mut c, CARENCIA + 10, 30).is_empty(),
            "no piso não há o que baixar"
        );
    }

    /// Rede que sarou devolve qualidade — até o que a pessoa pediu, e nunca
    /// além: a escolha dela é teto, não sugestão.
    #[test]
    fn rede_limpa_devolve_qualidade_ate_o_teto() {
        let mut c = Controlador::novo(8000);
        roda(&mut c, CARENCIA, 0);
        roda(&mut c, 2, 5);
        assert_eq!(c.atual(), 6000);

        let mut visto = 0;
        for _ in 0..12 {
            for d in roda(&mut c, LIMPAS_PARA_SUBIR + CARENCIA, 0) {
                if let Decisao::Muda { kbps, motivo } = d {
                    assert_eq!(motivo, Motivo::RedeLimpa);
                    visto = kbps;
                }
            }
        }
        assert!(visto > 0, "devia ter subido alguma vez");
        assert_eq!(c.atual(), 8000, "para exatamente no que a pessoa escolheu");
    }

    /// Subir e cair de novo aumenta a espera da próxima tentativa. Sem isto,
    /// uma rede que aguenta 6000 e não 7200 ficaria oscilando para sempre — e
    /// cada oscilação é uma interrupção para quem assiste.
    #[test]
    fn subida_frustrada_espaca_a_proxima() {
        let mut c = Controlador::novo(8000);
        roda(&mut c, CARENCIA, 0);
        roda(&mut c, 2, 5);

        let antes = c.limpas_exigidas();
        // Sobe com rede limpa, e a perda volta logo em seguida.
        roda(&mut c, LIMPAS_PARA_SUBIR + CARENCIA, 0);
        roda(&mut c, CARENCIA + 2, 5);
        let depois = c.limpas_exigidas();

        assert!(
            depois > antes,
            "a espera tem de crescer: {antes} -> {depois}"
        );
    }

    /// O recuo não pode crescer sem limite numa sessão longa em rede ruim.
    #[test]
    fn o_recuo_tem_teto() {
        let mut c = Controlador::novo(8000);
        for _ in 0..30 {
            roda(&mut c, LIMPAS_PARA_SUBIR + CARENCIA, 0);
            roda(&mut c, CARENCIA + 2, 5);
        }
        assert!(
            c.limpas_exigidas() <= LIMPAS_PARA_SUBIR * (1 << MAX_RECUOS),
            "espera explodiu: {}",
            c.limpas_exigidas()
        );
    }

    /// Uma queda tem de ser suficiente para o caso real. Com 8000 dando 16% de
    /// amostras com perda, 6000 é a primeira parada — e é o número que a gente
    /// mandaria a pessoa tentar à mão.
    #[test]
    fn a_primeira_queda_chega_no_numero_que_recomendariamos() {
        let mut c = Controlador::novo(8000);
        roda(&mut c, CARENCIA, 0);
        roda(&mut c, 2, 3);
        assert_eq!(c.atual(), 6000);
    }
}

#[cfg(test)]
mod testes_assume {
    use super::*;

    /// Uma troca decidida FORA do controlador — pela agenda de teste, ou pela
    /// escada de resolução no futuro — tem de virar a nova base dele.
    ///
    /// Sem isto, uma queda depois de uma troca externa para 6000 daria
    /// 8000 x 0,75 = 6000: a conta partiria da taxa que ele escolheu por
    /// último, não da que está no ar. O sintoma seria o controlador "não fazer
    /// nada" numa troca inteira, que é péssimo de diagnosticar.
    #[test]
    fn troca_externa_vira_a_nova_base() {
        let mut c = Controlador::novo(8000);
        c.assume(6000);
        assert_eq!(c.atual(), 6000);

        // Passa a carência e provoca uma queda.
        for _ in 0..CARENCIA {
            assert_eq!(c.amostra(0), Decisao::Manter);
        }
        c.amostra(5);
        match c.amostra(5) {
            Decisao::Muda { kbps, .. } => assert_eq!(kbps, 4500, "6000 x 0,75"),
            Decisao::Manter => panic!("devia ter baixado"),
        }
    }

    /// Também dá carência: o encoder novo precisa estabilizar, tenha a troca
    /// vindo de onde tiver vindo.
    #[test]
    fn troca_externa_da_carencia() {
        let mut c = Controlador::novo(8000);
        c.assume(6000);
        for _ in 0..CARENCIA {
            assert_eq!(c.amostra(30), Decisao::Manter, "carência vale para tudo");
        }
        assert_eq!(c.atual(), 6000);
    }

    /// O teto continua sendo o teto, e o piso o piso.
    #[test]
    fn troca_externa_respeita_os_limites() {
        let mut c = Controlador::novo(8000);
        c.assume(99_000);
        assert_eq!(c.atual(), 8000, "não passa do que a pessoa escolheu");
        c.assume(10);
        assert_eq!(c.atual(), PISO_KBPS, "não desce abaixo do piso");
    }
}
