import os
import asyncio
import json
from pathlib import Path
from web3 import Web3
from dotenv import load_dotenv
import requests
from py_builder_relayer_client.client import RelayClient
from py_builder_relayer_client.models import SafeTransaction, OperationType
from py_builder_signing_sdk.config import BuilderConfig
from py_builder_signing_sdk.sdk_types import BuilderApiKeyCreds
from src.redeem.constants import CTF_EXCHANGE_ADDRESS, USDC_ADDRESS, CTF_ABI, RELAYER_URL


load_dotenv()


class ClaimManager:
    def __init__(self):
        self.pk = os.getenv("POLYGON_PK") or ""
        self.api_key = os.getenv("POLY_API_KEY") or ""
        self.api_secret = os.getenv("POLY_API_SECRET") or ""
        self.api_passphrase = os.getenv("POLY_API_PASSPHRASE") or ""

        # Инициализация Relayer Client (для бесплатных транзакций)
        # Используем те же ключи, что и для торговли
        creds = BuilderApiKeyCreds(
            key=self.api_key,
            secret=self.api_secret,
            passphrase=self.api_passphrase
        )
        config = BuilderConfig(local_builder_creds=creds)

        self.client = RelayClient(
            relayer_url=RELAYER_URL,
            chain_id=137,
            private_key=self.pk,
            builder_config=config
        )

        # Web3 нужен только для кодирования данных (не для отправки)
        self.w3 = Web3()

        # Преобразуем адрес в Checksum формат
        self.ctf_address = Web3.to_checksum_address(CTF_EXCHANGE_ADDRESS)
        self.usdc_address = Web3.to_checksum_address(USDC_ADDRESS)

        self.contract = self.w3.eth.contract(address=self.ctf_address, abi=CTF_ABI)

    async def get_market_data(self, condition_id: str):
        """
        Получает данные маркета по condition_id через API Polymarket.
        Возвращает первый найденный маркет или None.
        """
        try:
            url = f"https://gamma-api.polymarket.com/markets?condition_ids={condition_id}"
            response = requests.get(url, timeout=10)
            response.raise_for_status()
            data = response.json()

            if isinstance(data, list) and len(data) > 0:
                return data[0]
            return None
        except Exception as e:
            print(f"❌ Ошибка при получении данных маркета: {e}")
            return None

    async def claim_winnings(self, condition_id: str, winning_outcome_index: int):
        """
        Отправляет транзакцию на клейм токенов.
        winning_outcome_index: 0 для YES/UP, 1 для NO/DOWN.
        """
        print(f"🏆 Попытка забрать выигрыш для Condition: {condition_id}...")

        try:
            # 1. Подготовка данных
            # parentCollectionId для обычных рынков всегда нули
            parent_collection_id = "0x" + "0" * 64

            # IndexSets - это битовая маска.
            # Если победил исход 0 (YES/UP), маска = 1 (в двоичном 01)
            # Если победил исход 1 (NO/DOWN), маска = 2 (в двоичном 10)
            index_set = [1 << winning_outcome_index]

            # 2. Кодируем вызов функции
            encoded_data = self.contract.encode_abi(
                abi_element_identifier="redeemPositions",
                args=[
                    self.usdc_address,
                    parent_collection_id,
                    condition_id,
                    index_set
                ]
            )

            # 3. Создаем транзакцию для Relayer
            safe_tx = SafeTransaction(
                to=self.ctf_address,
                data=encoded_data,  # type: ignore
                value="0",
                operation=OperationType.Call
            )

            # 4. Отправляем через Relayer (Gasless!)
            loop = asyncio.get_running_loop()
            response = await loop.run_in_executor(None, self.client.execute, [safe_tx])

            # Достаем transactionID из ответа
            tx_id = response.transaction_hash

            print(f"✅ Запрос на клейм отправлен! Task ID: {tx_id}")
            return True

        except Exception as e:
            print(f"❌ Ошибка при клейме: {e}")
            return False


def load_claim_data():
    """Загружает массив событий из claim.json"""
    claim_path = Path(__file__).parent / "claim.json"

    if not claim_path.exists():
        return []

    try:
        with open(claim_path, 'r') as f:
            data = json.load(f)

        # Поддерживаем как старый формат (объект), так и новый (массив)
        if isinstance(data, list):
            return data
        elif isinstance(data, dict) and "condition_id" in data:
            # Старый формат - оборачиваем в массив
            return [data]
        else:
            print("⚠️ Некорректный формат claim.json, возвращаем пустой массив")
            return []

    except json.JSONDecodeError as e:
        print(f"❌ Ошибка при чтении claim.json: {e}")
        return []


def save_claim_data(events):
    """Сохраняет массив событий в claim.json"""
    claim_path = Path(__file__).parent / "claim.json"

    try:
        with open(claim_path, 'w') as f:
            json.dump(events, f, indent=2)
        return True
    except Exception as e:
        print(f"❌ Ошибка при сохранении claim.json: {e}")
        return False


async def main():
    """Entry point для клейма наград - работает в бесконечном цикле"""
    print("=== Polymarket Auto-Claim Bot ===")
    print("🔄 Запущен в режиме автоматического клейма")
    print("📂 Проверка файла claim.json каждые 60 секунд\n")

    # Создаем ClaimManager один раз
    manager = ClaimManager()
    iteration = 0

    while True:
        iteration += 1
        print(f"\n--- Итерация #{iteration} ({asyncio.get_event_loop().time():.0f}s) ---")

        # Загружаем список событий из claim.json
        claim_events = load_claim_data()

        if not claim_events:
            print("📭 Очередь пуста. Ожидание новых событий...")
        else:
            print(f"📋 Найдено событий в очереди: {len(claim_events)}")

            # Список событий для удаления
            events_to_remove = []

            # Обрабатываем каждое событие
            for idx, event in enumerate(claim_events):
                # Проверяем формат события
                if "condition_id" not in event or "winning_outcome_index" not in event:
                    print(f"⚠️ Событие #{idx+1}: некорректный формат, пропускаем")
                    continue

                condition_id = event["condition_id"]
                stored_winner_index = event["winning_outcome_index"]

                print(f"\n🎯 Событие #{idx+1}:")
                print(f"   Condition ID: {condition_id[:16]}...{condition_id[-8:]}")

                # Получаем актуальные данные маркета через API
                market_data = await manager.get_market_data(condition_id)

                if not market_data:
                    print(f"   ⚠️ Не удалось получить данные маркета. Пропускаем.")
                    continue

                # Проверяем победителя по outcomePrices
                raw_prices = market_data.get("outcomePrices", "[]")
                try:
                    outcome_prices = json.loads(raw_prices)
                except:
                    outcome_prices = []

                # Определяем индекс победителя
                winner_index = -1
                if outcome_prices and len(outcome_prices) >= 2:
                    if outcome_prices[0] in ["1", "1.0"]:
                        winner_index = 0  # UP/YES
                    elif outcome_prices[1] in ["1", "1.0"]:
                        winner_index = 1  # DOWN/NO

                if winner_index == -1:
                    print(f"   ⏳ Событие еще не рассчитано (outcomePrices не определены). Ждем...")
                    continue

                outcome_name = "UP/YES" if winner_index == 0 else "DOWN/NO"
                print(f"   🏆 Победитель: {winner_index} ({outcome_name})")

                # Получаем токены
                tokens_json = market_data.get("clobTokenIds", "[]")
                try:
                    token_ids = json.loads(tokens_json)
                except:
                    token_ids = []

                if len(token_ids) < 2:
                    print(f"   ❌ Некорректные данные токенов. Удаляем из очереди.")
                    events_to_remove.append(event)
                    continue

                # Получаем ID токена победителя
                winning_token_id = token_ids[winner_index]

                if winning_token_id:

                    # Клеймим
                    success = await manager.claim_winnings(condition_id, winner_index)

                    if success:
                        print(f"   ✅ Клейм успешно отправлен! Удаляем из очереди.")
                        events_to_remove.append(event)

                # Небольшая пауза между событиями
                await asyncio.sleep(2)

            # Удаляем обработанные события
            if events_to_remove:
                print(f"\n🗑️ Удаляем {len(events_to_remove)} событий из очереди...")
                updated_events = [e for e in claim_events if e not in events_to_remove]

                if save_claim_data(updated_events):
                    print(f"💾 Файл обновлен. Осталось событий: {len(updated_events)}")
                else:
                    print("❌ Не удалось обновить файл claim.json")

        # Ждем 60 секунд перед следующей проверкой
        print(f"\n⏳ Ожидание 60 секунд до следующей проверки...")
        await asyncio.sleep(60)


if __name__ == "__main__":
    asyncio.run(main())
