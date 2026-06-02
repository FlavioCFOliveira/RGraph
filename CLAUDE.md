# CLAUDE.md — Diretrizes de Interação para o Projeto RGraph

## Propósito do Projeto

RGraph é uma crate Rust que fornece toda a infraestrutura de suporte a grafos. Foi desenhada desde o início para operar em ambientes de altíssima carga e alta concorrência.

O projeto suporta de raiz os dois modelos de dados de grafos mais relevantes:
- **LPG** (Label Property Graph)
- **RDF** (Resource Description Framework)

## 1. Tomada de Decisão

NÃO ESTÁS AUTORIZADO a tomar decisões sozinho.

Sempre que as instruções não sejam suficientes, claras, específicas, concretas, ou quando existirem contradições ou ambiguidades, DEVES **SEMPRE PERGUNTAR** ao utilizador como proceder.

Ao fazer perguntas ao utilizador:
- Fornece múltiplas opções (a, b, c, ...) indicando qual é a tua recomendação.
- Quando existirem múltiplas perguntas (necessidades de esclarecimento), coloca cada uma de forma sequencial (uma a uma).

## 2. Documentação

Toda a documentação do projeto deve estar escrita em **inglês** (no mais perfeito inglês, sem erros ortográficos, gramaticais, sintáticos, etc.).

Deves assumir uma linguagem técnica clara, simples e sem ambiguidades, destinada a utilizadores humanos.

A documentação deve ser **precisa e fiel ao código**.

## 3. Fluxo de Trabalho

O fluxo de trabalho deve seguir sempre os seguintes passos:

**Especificar → Implementar → Testar → Documentar**

---

## Política de Desenvolvimento Auto-Contido

Todos os ciclos de desenvolvimento devem ser auto-contidos. **NUNCA** deves fazer só parte de uma tarefa; cada desenvolvimento deve produzir um resultado.

Quando, no decorrer de uma tarefa, forem descobertas necessidades novas que não foram antes previstas, essas novas necessidades devem ser resolvidas (da forma mais imediata possível) no mesmo ciclo de desenvolvimento (adicionadas novas tarefas e desenvolvidas tão rapidamente quanto possível).

Todo o código ou desenvolvimento deve ser por regra **full-fledged**. Não devem ser criados testes com `skip`.

Sempre que encontrares bugs pre-existentes, deves corrigi-los na hora e continuar o trabalho que estavas a fazer quando encontraste o bug.

---

## Orientado para Produção

Em todo o ciclo de trabalho (análise → planeamento → desenvolvimento → testes) deve estar como objetivo que o resultado produzido deve ser **production-grade**.

Deves usar não só o máximo de conhecimento como também o máximo empenho para garantir que cada trabalho produz código pronto para ser usado em produção.

---

## Planeamento e Execução de Tarefas

Para fazer o planeamento e coordenar a execução, deves usar a ferramenta **`rmp`** (CLI disponível no sistema para gestão de roadmap).

Deves considerar esta ferramenta como **fonte única de verdade** no âmbito do planeamento e execução das tarefas deste projeto. Nenhuma outra forma deve ser usada para este fim.

Usa o **Knowledge Graph** para melhor compreender o projeto, os seus componentes e a forma como eles se relacionam, a fim de ser mais fácil identificar o âmbito e o impacto de cada uma das tarefas no projeto.

### Planeamento

Deves observar atentamente o âmbito do trabalho proposto pelo utilizador e determinar primariamente se faz sentido existirem várias fases de desenvolvimento a fim de acomodar devidamente as tarefas. Considera que cada fase deve acomodar um entregável (deliverable) sólido.

Todas as tarefas devem ter uma definição muito clara e objetiva dos seus objetivos, requisitos funcionais e requisitos técnicos, tal como também deve conter quais são os critérios de aceitação que confirmam que uma tarefa pode ser concluída (que objetivo está cumprido).

Sempre que uma tareja for concluída, a tarefa deve ser fechada com um pequeno sumário resumindo o que foi feito.

As fases devem ser consideradas "Sprints" na ferramenta `rmp`, que servem para agrupar tarefas.

Se o trabalho que está a ser planeado necessitar de várias fases (ou sprints), então o planeamento deve comportar duas fases distintas:
1. Primeiro, definir quais são as fases (ou sprints) necessárias e qual é o âmbito (objetivo de cada sprint).
2. Só depois, percorrer sprint a sprint para determinar quais são as tarefas de cada sprint.

Usa sempre a ferramenta `rmp` como fonte única de verdade.

Usa o **Knowledge Graph** para ajudar a perceber quais são as tarefas com mais ganhos e qual é a extensão dos impactos de cada tarefa. Usa o KG (Knowledge Graph) para ajudar a determinar quais as tarefas fundacionais e de maior ganho a fim de otimizares o melhor caminho para a execução das tarefas.

### Execução de Tarefas

A execução de tarefas é a continuação natural (o passo seguinte) ao planeamento. Deves usar sempre a ferramenta `rmp` para determinar:

1. Se existe alguma tarefa aberta que ainda não esteja concluída para dar seguimento à mesma.
2. Identificar qual é a próxima tarefa.
3. Identificar e compreender o objetivo da tarefa que vai ser iniciada com base na descrição, nos requisitos funcionais e técnicos.
4. Validar sempre se os critérios de aceitação são observados antes de fechar a tarefa.
5. Garantir que a tarefa é fechada incorporando um pequeno sumário do que foi feito.
6. Depois da tarefa ser fechada e antes de seguir para a próxima, deve ser feito um `git commit` seguindo as boas práticas, explicando o que foi feito.
7. Atualizar o Knowledge Graph.

A execução das tarefas e dos sprints deve preferencialmente ser executada de forma sequencial. Os sprints só podem ser executados de forma sequencial; as tarefas podem ocorrer de forma paralela se houver justificação para isso.

---

## Knowledge Graph

Deves usar as funcionalidades "Graph" do `rmp` (Groadmap) para criar, manter (atualizar) e consultar um grafo de conhecimento do projeto.

Este grafo **DEVE TER TUDO** o que se revele útil saber sobre o projeto (exemplos: que funcionalidades tem, onde estão especificadas, onde estão implementadas, quais são os testes existentes, o que testam, quais são os componentes, como se relacionam, quais as dependências entre eles, em que git commit a funcionalidade foi especificada, em que git commit a funcionalidade foi implementada e em qual foi testada, quais as tarefas de rmp, tarefas de componentes, etc.), entre outras informações que possam ser úteis mapear.

Este grafo de conhecimentos **DEVE SEMPRE SER ATUALIZADO** em cada `git commit`, indicando as alterações nos objetos do grafo. Ao atualizar os nós e as relações, deve ficar identificado qual foi o commit e a data do mesmo.

**Este grafo tem o objetivo de providenciar a verdade absoluta sobre o projeto.** DEVES zelar de forma muito focada por o manter sempre o mais atualizado possível, para que, antes de teres de ler ficheiros, possas consultar o grafo e saber o que precisas.

Podes criar os nós e arestas que fizerem mais sentido para o projeto e para a tua atividade. Usa o grafo em conjunto com as tarefas e sprints para coordenar os trabalhos do projeto.

---

## Nunca Advinhar

Todas as interações no projeto devem ser baseadas **EXCLUSIVAMENTE** nos conhecimentos que já tens, e nunca tentar adivinhar as respostas pretendidas.

Quando a informação que tens não é suficiente, deves procurar as respostas na internet em fontes oficiais ou autoritativas, papers, livros ou autores da especialidade a fim de determinar qual o melhor resultado.

---

## Medir para Decidir

Sempre que for necessário avaliar o desempenho (performance), a completude (se está completo), ou a assertividade (se está correto), deve-se **SEMPRE** recolher evidências do projeto para determinar as necessidades.

Deves decidir de forma empírica.
